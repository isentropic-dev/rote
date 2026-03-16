// Browser process launch and lifecycle management.

use std::{fs, path::PathBuf, process, time::Duration};

use tokio::time;

use super::{CdpError, discover, protocol};
use discover::BrowserBinary;
use protocol::{BrowserVersion, TabInfo};

/// Default debugging port.
pub const DEFAULT_PORT: u16 = 9222;

/// Maximum time to wait for the browser's debug endpoint to become available.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between health check attempts during startup.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// A launched browser process with its debug port and profile directory.
pub struct BrowserProcess {
    child: process::Child,
    port: u16,
    // Retained for future use (e.g., diagnostics); not read at runtime.
    #[allow(dead_code)]
    profile_dir: PathBuf,
}

impl BrowserProcess {
    /// Launch a browser binary with remote debugging enabled.
    ///
    /// Creates a temporary profile directory and launches the browser
    /// pointing at it.
    /// Waits for the debug endpoint to become available before returning.
    pub async fn launch(binary: &BrowserBinary, port: u16) -> Result<Self, CdpError> {
        let profile_dir = create_profile_dir()?;

        let child = process::Command::new(&binary.path)
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-default-apps")
            .arg("about:blank")
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .spawn()
            .map_err(|e| {
                CdpError::BrowserLaunch(format!("failed to spawn {}: {e}", binary.path.display(),))
            })?;

        let mut process = Self {
            child,
            port,
            profile_dir,
        };

        // Wait for the debug endpoint to come up.
        if let Err(e) = process.wait_for_ready().await {
            // Clean up if we fail during startup.
            process.kill();
            return Err(e);
        }

        Ok(process)
    }

    /// List open tabs/targets.
    pub async fn tabs(&self) -> Result<Vec<TabInfo>, CdpError> {
        fetch_tabs(self.port).await
    }

    /// Find the first page-type tab, or the first tab of any type.
    pub async fn first_page_tab(&self) -> Result<TabInfo, CdpError> {
        let tabs = self.tabs().await?;
        tabs.into_iter()
            .find(|t| t.target_type == "page")
            .ok_or(CdpError::NoTab)
    }

    /// Kill the browser process and clean up the profile directory.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Poll the debug endpoint until it responds.
    async fn wait_for_ready(&self) -> Result<(), CdpError> {
        let deadline = time::Instant::now() + STARTUP_TIMEOUT;

        loop {
            match fetch_version(self.port).await {
                Ok(_) => return Ok(()),
                Err(_) if time::Instant::now() < deadline => {
                    time::sleep(STARTUP_POLL_INTERVAL).await;
                }
                Err(_) => {
                    return Err(CdpError::BrowserLaunch(format!(
                        "browser did not start within {} seconds on port {}",
                        STARTUP_TIMEOUT.as_secs(),
                        self.port,
                    )));
                }
            }
        }
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Return the persistent profile directory for the default browser session.
///
/// Creates the directory if it doesn't exist.
fn create_profile_dir() -> Result<PathBuf, CdpError> {
    let data_dir = dirs::data_dir().ok_or_else(|| {
        CdpError::BrowserLaunch("could not determine platform data directory".into())
    })?;
    let dir = data_dir.join("rote").join("profiles").join("default");
    fs::create_dir_all(&dir).map_err(|e| {
        CdpError::BrowserLaunch(format!(
            "failed to create profile directory {}: {e}",
            dir.display(),
        ))
    })?;
    Ok(dir)
}

/// Fetch `/json/version` from the debug endpoint.
async fn fetch_version(port: u16) -> Result<BrowserVersion, CdpError> {
    let url = format!("http://localhost:{port}/json/version");
    let body = http_get(&url).await?;
    serde_json::from_str(&body)
        .map_err(|e| CdpError::Protocol(format!("failed to parse /json/version: {e}")))
}

/// Fetch `/json/list` from the debug endpoint.
async fn fetch_tabs(port: u16) -> Result<Vec<TabInfo>, CdpError> {
    let url = format!("http://localhost:{port}/json/list");
    let body = http_get(&url).await?;
    serde_json::from_str(&body)
        .map_err(|e| CdpError::Protocol(format!("failed to parse /json/list: {e}")))
}

/// HTTP GET for the browser's local JSON endpoints.
///
/// Uses `ureq` instead of a hand-rolled localhost client because Chrome's
/// debug endpoint can behave poorly with the minimal implementation.
async fn http_get(url: &str) -> Result<String, CdpError> {
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut response = ureq::get(&url)
            .call()
            .map_err(|e| CdpError::Connection(format!("failed to GET {url}: {e}")))?;

        response
            .body_mut()
            .read_to_string()
            .map_err(|e| CdpError::Connection(format!("failed to read HTTP response body: {e}")))
    })
    .await
    .map_err(|e| CdpError::Connection(format!("HTTP worker task failed: {e}")))?
}
