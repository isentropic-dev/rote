// Browser binary discovery across platforms.

use std::path::PathBuf;

/// A browser we can control via CDP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKind {
    Chrome,
    Edge,
}

impl BrowserKind {
    /// Human-readable name.
    #[cfg(test)]
    pub fn name(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Google Chrome",
            BrowserKind::Edge => "Microsoft Edge",
        }
    }
}

/// A discovered browser installation.
#[derive(Debug, Clone)]
pub struct BrowserBinary {
    #[cfg_attr(not(test), allow(dead_code))]
    pub kind: BrowserKind,
    pub path: PathBuf,
}

/// Search order: Chrome first, then Edge.
/// Returns all candidates that exist on the filesystem.
pub fn find_browsers() -> Vec<BrowserBinary> {
    let candidates = candidate_paths();
    candidates.into_iter().filter(|b| b.path.exists()).collect()
}

/// Find the first available browser.
pub fn find_browser() -> Option<BrowserBinary> {
    find_browsers().into_iter().next()
}

/// Platform-specific candidate paths.
/// Chrome paths come first so Chrome is preferred over Edge.
fn candidate_paths() -> Vec<BrowserBinary> {
    let mut paths = Vec::new();

    #[cfg(target_os = "macos")]
    {
        paths.extend(macos_candidates());
    }

    #[cfg(target_os = "linux")]
    {
        paths.extend(linux_candidates());
    }

    #[cfg(target_os = "windows")]
    {
        paths.extend(windows_candidates());
    }

    paths
}

#[cfg(target_os = "macos")]
fn macos_candidates() -> Vec<BrowserBinary> {
    use BrowserKind::{Chrome, Edge};

    vec![
        BrowserBinary {
            kind: Chrome,
            path: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        },
        BrowserBinary {
            kind: Edge,
            path: "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
        },
    ]
}

#[cfg(target_os = "linux")]
fn linux_candidates() -> Vec<BrowserBinary> {
    use BrowserKind::{Chrome, Edge};

    vec![
        BrowserBinary {
            kind: Chrome,
            path: "google-chrome".into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: "google-chrome-stable".into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: "/usr/bin/google-chrome".into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: "/usr/bin/google-chrome-stable".into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: "chromium".into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: "chromium-browser".into(),
        },
        BrowserBinary {
            kind: Edge,
            path: "microsoft-edge".into(),
        },
        BrowserBinary {
            kind: Edge,
            path: "microsoft-edge-stable".into(),
        },
        BrowserBinary {
            kind: Edge,
            path: "/usr/bin/microsoft-edge".into(),
        },
    ]
}

#[cfg(target_os = "windows")]
fn windows_candidates() -> Vec<BrowserBinary> {
    use BrowserKind::{Chrome, Edge};

    let program_files =
        std::env::var("PROGRAMFILES").unwrap_or_else(|_| r"C:\Program Files".into());
    let program_files_x86 =
        std::env::var("PROGRAMFILES(X86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    let local_app_data =
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| r"C:\Users\Default\AppData\Local".into());

    vec![
        BrowserBinary {
            kind: Chrome,
            path: format!(r"{program_files}\Google\Chrome\Application\chrome.exe").into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: format!(r"{program_files_x86}\Google\Chrome\Application\chrome.exe").into(),
        },
        BrowserBinary {
            kind: Chrome,
            path: format!(r"{local_app_data}\Google\Chrome\Application\chrome.exe").into(),
        },
        BrowserBinary {
            kind: Edge,
            path: format!(r"{program_files}\Microsoft\Edge\Application\msedge.exe").into(),
        },
        BrowserBinary {
            kind: Edge,
            path: format!(r"{program_files_x86}\Microsoft\Edge\Application\msedge.exe").into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_not_empty() {
        let candidates = candidate_paths();
        assert!(
            !candidates.is_empty(),
            "should have at least one candidate path on every platform",
        );
    }

    #[test]
    fn chrome_comes_before_edge() {
        let candidates = candidate_paths();
        let first_chrome = candidates
            .iter()
            .position(|b| b.kind == BrowserKind::Chrome);
        let first_edge = candidates.iter().position(|b| b.kind == BrowserKind::Edge);
        if let (Some(c), Some(e)) = (first_chrome, first_edge) {
            assert!(c < e, "Chrome should be checked before Edge");
        }
    }

    #[test]
    fn browser_kind_names() {
        assert_eq!(BrowserKind::Chrome.name(), "Google Chrome");
        assert_eq!(BrowserKind::Edge.name(), "Microsoft Edge");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_has_expected_paths() {
        let candidates = candidate_paths();
        let paths: Vec<_> = candidates
            .iter()
            .map(|b| b.path.to_str().unwrap())
            .collect();
        assert!(paths.iter().any(|p| p.contains("Google Chrome")));
        assert!(paths.iter().any(|p| p.contains("Microsoft Edge")));
    }
}
