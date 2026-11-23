//! HTTP for `--addons` and `--update`: a GitHub release's JSON, and a download with its progress
//! on a terminal.
use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::time::Duration;

/// A launcher.conf value naming a release: an API URL as it is, `OWNER/REPO` the latest release there.
pub fn repo_url(value: &str) -> String {
    if value.contains("://") {
        value.to_string()
    } else {
        format!("https://api.github.com/repos/{}/releases/latest", value.trim_matches('/'))
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(concat!("collabo-core-launcher/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .build(),
    )
}

/// `key`: the launcher.conf key that names the release, for a 404.
fn http_error(url: &str, error: ureq::Error, key: &str) -> String {
    match error {
        ureq::Error::StatusCode(403 | 429) => format!("{url}: refused (GitHub allows 60 requests an hour without a login; try again later)"),
        ureq::Error::StatusCode(404) => format!("{url}: not found ({key} in launcher.conf)"),
        error => format!("{url}: {error}"),
    }
}

/// The release at `url` (the GitHub API's JSON).
pub fn release(url: &str, key: &str) -> Result<serde_json::Value, String> {
    let mut response =
        agent().get(url).header("accept", "application/vnd.github+json").call().map_err(|e| http_error(url, e, key))?;
    let text = response.body_mut().read_to_string().map_err(|e| format!("{url}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{url}: {e}"))
}

pub fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// Downloads `url` into `to`, showing `label` and the progress (of `size` when the server does
/// not say) on a terminal.
pub fn download(url: &str, label: &str, size: u64, to: &Path) -> Result<(), String> {
    let mut response = agent().get(url).call().map_err(|e| http_error(url, e, "the release"))?;
    let total = response.body().content_length().unwrap_or(size);
    let mut body = response.body_mut().as_reader();
    let mut file = File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let progress = std::io::stderr().is_terminal();
    let (mut buffer, mut done, mut shown) = (vec![0u8; 256 * 1024], 0u64, 0u64);
    loop {
        let n = body.read(&mut buffer).map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n]).map_err(|e| format!("{}: {e}", to.display()))?;
        done += n as u64;
        if progress && (done - shown >= 1 << 20 || done == total) {
            shown = done;
            eprint!("\r{label}: {} / {}  ", megabytes(done), megabytes(total));
        }
    }
    file.flush().map_err(|e| format!("{}: {e}", to.display()))?;
    if progress {
        eprint!("\r\x1b[K");
    }
    if total > 0 && done != total {
        return Err(format!("{url}: {} of {} arrived (try again)", megabytes(done), megabytes(total)));
    }
    eprintln!("{label}: downloaded {}", megabytes(done));
    Ok(())
}
