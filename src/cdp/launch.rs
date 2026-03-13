// Browser process launch and lifecycle management.

use std::{fs, path::PathBuf, process, time::Duration};

use tokio::{net::TcpStream, time};
// Trait imports: needed for `.read_to_string()` and `.write_all()` on async streams.
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    /// The debugging port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Fetch the browser version info from the debug endpoint.
    pub async fn version(&self) -> Result<BrowserVersion, CdpError> {
        fetch_version(self.port).await
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
        // Best-effort cleanup of the profile directory.
        let _ = fs::remove_dir_all(&self.profile_dir);
    }
}

/// Create a temporary profile directory for the browser session.
fn create_profile_dir() -> Result<PathBuf, CdpError> {
    let dir = std::env::temp_dir().join(format!("rote-profile-{}", process::id()));
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

/// Minimal HTTP GET using a raw TCP connection.
///
/// We avoid pulling in a full HTTP client (reqwest/hyper) for these
/// two simple JSON endpoints.
/// The debug endpoint always returns the full body in one response.
async fn http_get(url: &str) -> Result<String, CdpError> {
    // Parse the URL minimally — we only ever hit localhost.
    let url = url
        .strip_prefix("http://")
        .ok_or_else(|| CdpError::Protocol(format!("invalid URL: {url}")))?;

    let (host_port, path) = url
        .split_once('/')
        .map(|(hp, p)| (hp, format!("/{p}")))
        .unwrap_or((url, "/".into()));

    let mut stream = TcpStream::connect(host_port)
        .await
        .map_err(|e| CdpError::Connection(format!("failed to connect to {host_port}: {e}")))?;

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| CdpError::Connection(format!("failed to write HTTP request: {e}")))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .map_err(|e| CdpError::Connection(format!("failed to read HTTP response: {e}")))?;

    // Split headers from body.
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or(response);

    Ok(body)
}
