// Recorder JS bridge — connects CDP events to the training core.
//
// Provides the recorder JS that gets injected into browser pages,
// a parser for CDP console events, and injection helpers.

use serde_json::Value;

use super::{Command, SelectorInfo};

/// The recorder JS source code, injected into browser pages to capture
/// user interactions and emit structured JSON via `console.log`.
pub const RECORDER_SCRIPT: &str = r##"
(function() {
  if (window.__roteRecorder) return;
  window.__roteRecorder = true;

  function getCssPath(el) {
    if (el.id) return "#" + CSS.escape(el.id);

    const parts = [];
    let current = el;

    while (current && current.nodeType === Node.ELEMENT_NODE) {
      let selector = current.tagName.toLowerCase();

      if (current.id) {
        selector = "#" + CSS.escape(current.id);
        parts.unshift(selector);
        break;
      }

      if (current.className && typeof current.className === "string") {
        const classes = current.className.trim().split(/\s+/).filter(c => c);
        if (classes.length > 0) {
          selector += "." + classes.map(c => CSS.escape(c)).join(".");
        }
      }

      const parent = current.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter(
          c => c.tagName === current.tagName
        );
        if (siblings.length > 1) {
          const index = siblings.indexOf(current) + 1;
          selector += ":nth-of-type(" + index + ")";
        }
      }

      parts.unshift(selector);
      current = current.parentElement;
    }

    return parts.join(" > ");
  }

  function getXPath(el) {
    if (el.id) return '//*[@id="' + el.id + '"]';

    const parts = [];
    let current = el;

    while (current && current.nodeType === Node.ELEMENT_NODE) {
      let tag = current.tagName.toLowerCase();

      if (current.id) {
        parts.unshift('//*[@id="' + current.id + '"]');
        break;
      }

      const parent = current.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter(
          c => c.tagName === current.tagName
        );
        if (siblings.length > 1) {
          const index = siblings.indexOf(current) + 1;
          tag += "[" + index + "]";
        }
      }

      parts.unshift(tag);
      current = current.parentElement;
    }

    return "//" + parts.join("/");
  }

  function getSelectors(el) {
    return {
      id: el.id || null,
      css: getCssPath(el),
      xpath: getXPath(el),
      textContent: (el.textContent || "").trim().slice(0, 50) || null
    };
  }

  document.addEventListener("click", (e) => {
    const el = e.target;
    console.log(JSON.stringify({
      __rote: true,
      type: "click",
      tagName: el.tagName,
      selector: getSelectors(el)
    }));
  }, true);

  document.addEventListener("input", (e) => {
    const el = e.target;
    console.log(JSON.stringify({
      __rote: true,
      type: "input",
      tagName: el.tagName,
      value: el.value,
      selector: getSelectors(el)
    }));
  }, true);
})();
"##;

/// CDP parameters for `Page.addScriptToEvaluateOnNewDocument`.
///
/// When sent via CDP, this auto-injects the recorder on every new document.
#[must_use]
pub fn auto_inject_params() -> Value {
    serde_json::json!({
        "source": RECORDER_SCRIPT
    })
}

/// Parse a `Runtime.consoleAPICalled` event into a `Command`, if it's from our recorder.
///
/// Returns `None` for non-recorder console messages.
#[must_use]
pub fn parse_recorder_event(params: &Value) -> Option<Command> {
    // Only process "log" type console calls.
    let call_type = params.get("type")?.as_str()?;
    if call_type != "log" {
        return None;
    }

    // Extract the first argument's value.
    let args = params.get("args")?.as_array()?;
    let first_arg = args.first()?;
    let value_str = first_arg.get("value")?.as_str()?;

    // Try to parse as JSON.
    let obj: Value = serde_json::from_str(value_str).ok()?;

    // Check for the rote marker.
    if !obj.get("__rote")?.as_bool()? {
        return None;
    }

    let event_type = obj.get("type")?.as_str()?;
    let tag_name = obj.get("tagName")?.as_str()?.to_owned();

    let selector_obj = obj.get("selector")?;
    let selector_info = SelectorInfo {
        id: selector_obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        css: selector_obj
            .get("css")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        xpath: selector_obj
            .get("xpath")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        text_content: selector_obj
            .get("textContent")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
    };

    match event_type {
        "click" => Some(Command::BrowserClick {
            selector_info,
            tag: tag_name,
        }),
        "input" => {
            let value = obj.get("value")?.as_str()?.to_owned();
            Some(Command::BrowserInput {
                selector_info,
                tag: tag_name,
                value,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console_event(json_str: &str) -> Value {
        serde_json::json!({
            "type": "log",
            "args": [{
                "type": "string",
                "value": json_str
            }],
            "executionContextId": 1,
            "timestamp": 1234.0
        })
    }

    #[test]
    fn parse_click_event() {
        let json = r##"{"__rote":true,"type":"click","tagName":"BUTTON","selector":{"id":"submit-btn","css":"#submit-btn","xpath":"//*[@id=\"submit-btn\"]","textContent":"Submit"}}"##;
        let params = console_event(json);
        let cmd = parse_recorder_event(&params).unwrap();

        match cmd {
            Command::BrowserClick { selector_info, tag } => {
                assert_eq!(tag, "BUTTON");
                assert_eq!(selector_info.id.as_deref(), Some("submit-btn"));
                assert_eq!(selector_info.css.as_deref(), Some("#submit-btn"));
                assert_eq!(selector_info.text_content.as_deref(), Some("Submit"));
            }
            _ => panic!("expected BrowserClick"),
        }
    }

    #[test]
    fn parse_input_event() {
        let json = r##"{"__rote":true,"type":"input","tagName":"INPUT","value":"Alice","selector":{"id":"name-field","css":"#name-field","xpath":"//*[@id=\"name-field\"]"}}"##;
        let params = console_event(json);
        let cmd = parse_recorder_event(&params).unwrap();

        match cmd {
            Command::BrowserInput {
                selector_info,
                tag,
                value,
            } => {
                assert_eq!(tag, "INPUT");
                assert_eq!(value, "Alice");
                assert_eq!(selector_info.id.as_deref(), Some("name-field"));
            }
            _ => panic!("expected BrowserInput"),
        }
    }

    #[test]
    fn non_recorder_message_ignored() {
        let params = console_event(r##"{"message": "hello world"}"##);
        assert!(parse_recorder_event(&params).is_none());
    }

    #[test]
    fn non_log_type_ignored() {
        let params = serde_json::json!({
            "type": "error",
            "args": [{
                "type": "string",
                "value": r##"{"__rote":true,"type":"click","tagName":"BUTTON","selector":{"id":"x","css":"#x","xpath":"//x"}}"##
            }]
        });
        assert!(parse_recorder_event(&params).is_none());
    }

    #[test]
    fn plain_text_message_ignored() {
        let params = console_event("rote: recorder installed");
        assert!(parse_recorder_event(&params).is_none());
    }

    #[test]
    fn auto_inject_params_has_source() {
        let params = auto_inject_params();
        assert!(params.get("source").is_some());
        let source = params["source"].as_str().unwrap();
        assert!(source.contains("__roteRecorder"));
    }

    #[test]
    fn null_selector_fields_handled() {
        let json = r##"{"__rote":true,"type":"click","tagName":"DIV","selector":{"id":null,"css":"div.content","xpath":"//div","textContent":null}}"##;
        let params = console_event(json);
        let cmd = parse_recorder_event(&params).unwrap();

        match cmd {
            Command::BrowserClick { selector_info, tag } => {
                assert_eq!(tag, "DIV");
                assert!(selector_info.id.is_none());
                assert_eq!(selector_info.css.as_deref(), Some("div.content"));
                assert!(selector_info.text_content.is_none());
            }
            _ => panic!("expected BrowserClick"),
        }
    }
}
