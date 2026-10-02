//! Fetching a URL with `curl` (or `wget`), as `decdrm models download` fetches the
//! DAC weights (`decdrm_dac::weights::download_weights`): DecDRM has no HTTP
//! client of its own. Windows 10/11 ship `curl.exe`; Linux distributions have one of
//! the two. Called only when the user asks for an update.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Why a download failed.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error(
        "neither curl nor wget could be run; download {url} in a browser and save it as {path}"
    )]
    NoTool { url: String, path: PathBuf },
    #[error("{tool} could not download {url}: {message}")]
    Tool {
        tool: &'static str,
        url: String,
        message: String,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{url} gave no {format} schedule entries (an error page?); the previous file was kept")]
    NotASchedule { url: String, format: crate::Format },
}

/// Download `url` into the file `dest` (overwritten). curl and wget run quietly with
/// their output captured, so their error message can be reported (and, on Windows, no
/// console window opens when the GUI runs them).
pub fn fetch(url: &str, dest: &Path) -> Result<(), DownloadError> {
    let tools: [(&'static str, Vec<&str>); 2] = [
        (
            "curl",
            vec![
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--retry",
                "2",
                "--connect-timeout",
                "20",
                "--max-time",
                "300",
                "--output",
            ],
        ),
        (
            "wget",
            vec!["--quiet", "--tries=2", "--timeout=30", "--output-document"],
        ),
    ];
    for (tool, args) in tools {
        let mut cmd = Command::new(tool);
        cmd.args(&args).arg(dest).arg(url);
        no_console_window(&mut cmd);
        // `output()` runs the program (found on PATH) to completion, capturing stdout and
        // stderr; `NotFound` means it is not installed.
        match cmd.output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let message = match stderr.trim().lines().last() {
                    Some(line) if !line.trim().is_empty() => {
                        format!("{} ({})", line.trim(), out.status)
                    }
                    _ => out.status.to_string(),
                };
                return Err(DownloadError::Tool {
                    tool,
                    url: url.to_string(),
                    message,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(DownloadError::Io {
                    path: PathBuf::from(tool),
                    source,
                });
            }
        }
    }
    Err(DownloadError::NoTool {
        url: url.to_string(),
        path: dest.to_path_buf(),
    })
}

/// On Windows, start the program without a console window (`CREATE_NO_WINDOW`): a GUI
/// program has no console, and Windows would open one for curl.
#[cfg(windows)]
fn no_console_window(cmd: &mut Command) {
    // Rust note: `CommandExt` is a Windows-only extension trait; bringing it into scope
    // adds `creation_flags` to `Command`.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_cmd: &mut Command) {}
