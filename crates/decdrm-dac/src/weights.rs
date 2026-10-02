//! Where the DAC model weights live, and fetching them.
//!
//! The weights are Descript's `descript/dac_24khz` model from Hugging Face (MIT
//! licence), the `model.safetensors` of a pinned revision (299 MB, float32). They are
//! never committed; each installation downloads them once:
//!
//! ```text
//! decdrm models download dac            # into the default models directory
//! decdrm models download dac --dir D    # into D/dac_24khz/model.safetensors
//! ```
//!
//! **Models directory**: `$DECDRM_MODELS` if that environment variable is set,
//! otherwise `models` next to the executable. The weights are
//! `<models>/dac_24khz/model.safetensors`.
//!
//! **Lookup** ([`find_weights`]): with `$DECDRM_MODELS` set, only there. Otherwise
//! `models/dac_24khz/model.safetensors` in the executable's directory and up to four
//! of its parents, so that development builds (`target/release/decdrm`, test binaries in
//! `target/debug/deps`) find a `models` directory at the workspace root. Builds with the
//! `embed-weights` feature (single-file portable executables) fall back to the weights
//! built into them ([`EMBEDDED_WEIGHTS`]).

use crate::sha256::Sha256;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment variable naming the models directory.
pub const MODELS_ENV: &str = "DECDRM_MODELS";
/// Subdirectory of the models directory for the DAC model.
pub const MODEL_NAME: &str = "dac_24khz";
/// Weights file name.
pub const WEIGHTS_FILE: &str = "model.safetensors";
/// Download URL of the weights (pinned revision of `descript/dac_24khz`).
pub const WEIGHTS_URL: &str =
    "https://huggingface.co/descript/dac_24khz/resolve/6ba020b5ba7d9d8076fb90db7e67f27e31980f6e/model.safetensors";
/// Size of the weights file, bytes.
pub const WEIGHTS_SIZE: u64 = 298_652_268;
/// SHA-256 of the weights file.
pub const WEIGHTS_SHA256: &str = "7452e3fc6972991da871ae1a1c9d3e8e219aa247cc0f757018867d6a7fe59aaf";

/// Parent directories of the executable searched besides its own.
const PARENT_LEVELS: usize = 4;

/// The weights built into the executable (`embed-weights` feature), else `None`.
#[cfg(feature = "embed-weights")]
pub static EMBEDDED_WEIGHTS: Option<&[u8]> = Some(include_bytes!(env!("DECDRM_EMBEDDED_WEIGHTS")));
/// The weights built into the executable (`embed-weights` feature), else `None`.
#[cfg(not(feature = "embed-weights"))]
pub static EMBEDDED_WEIGHTS: Option<&[u8]> = None;

/// What [`find_weights`] returns for the built-in weights (not a file).
pub const EMBEDDED_PATH: &str = "<built into the executable>";

/// Whether `path` stands for the built-in weights.
pub fn is_embedded(path: &Path) -> bool {
    EMBEDDED_WEIGHTS.is_some() && path == Path::new(EMBEDDED_PATH)
}

/// `$DECDRM_MODELS`, if set and not empty.
fn env_models_dir() -> Option<PathBuf> {
    std::env::var_os(MODELS_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The directory downloads go to: `$DECDRM_MODELS`, else `models` next to the
/// executable.
pub fn default_models_dir() -> Option<PathBuf> {
    env_models_dir().or_else(|| Some(std::env::current_exe().ok()?.parent()?.join("models")))
}

/// The weights file inside a models directory.
pub fn weights_path(models_dir: &Path) -> PathBuf {
    models_dir.join(MODEL_NAME).join(WEIGHTS_FILE)
}

/// Every place [`find_weights`] looks, in order.
pub fn candidate_paths() -> Vec<PathBuf> {
    if let Some(dir) = env_models_dir() {
        return vec![weights_path(&dir)];
    }
    let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else {
        return Vec::new();
    };
    // `ancestors()` yields the directory itself, then each parent.
    exe_dir.ancestors().take(PARENT_LEVELS + 1).map(|d| weights_path(&d.join("models"))).collect()
}

/// The weights could not be found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeightsNotFound {
    /// The paths that were tried.
    pub searched: Vec<PathBuf>,
}

impl std::fmt::Display for WeightsNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DAC model weights not found")?;
        match self.searched.as_slice() {
            [] => {}
            [one] => write!(f, " at {}", one.display())?,
            [first, .., last] => write!(f, " (looked in {} … {})", first.display(), last.display())?,
        }
        write!(
            f,
            "; run `decdrm models download dac`, or set {MODELS_ENV} to the directory containing \
             {MODEL_NAME}/{WEIGHTS_FILE}"
        )
    }
}

impl std::error::Error for WeightsNotFound {}

/// The first existing weights file of [`candidate_paths`], else [`EMBEDDED_PATH`] when
/// the weights are built in.
pub fn find_weights() -> Result<PathBuf, WeightsNotFound> {
    let searched = candidate_paths();
    if let Some(p) = searched.iter().find(|p| p.is_file()) {
        return Ok(p.clone());
    }
    if EMBEDDED_WEIGHTS.is_some() {
        return Ok(PathBuf::from(EMBEDDED_PATH));
    }
    Err(WeightsNotFound { searched })
}

/// Why fetching or checking the weights failed.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("neither curl nor wget could be run; download {url} by hand and save it as {path}")]
    NoTool { url: &'static str, path: PathBuf },
    #[error("{tool} failed ({status}) downloading {url}")]
    Tool { tool: &'static str, status: String, url: &'static str },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} has {got} bytes, the DAC weights have {want}")]
    Size { path: PathBuf, got: u64, want: u64 },
    #[error("{path}: SHA-256 {got} differs from the expected {want}")]
    Checksum { path: PathBuf, got: String, want: &'static str },
}

/// Check size and SHA-256 of a weights file (or of the built-in weights).
pub fn verify_weights(path: &Path) -> Result<(), DownloadError> {
    let io = |source| DownloadError::Io { path: path.to_path_buf(), source };
    if let (true, Some(bytes)) = (is_embedded(path), EMBEDDED_WEIGHTS) {
        return check_weights(path, bytes.len() as u64, bytes);
    }
    let got = std::fs::metadata(path).map_err(io)?.len();
    check_weights(path, got, std::fs::File::open(path).map_err(io)?)
}

/// Check the size `got` and the SHA-256 of weights read from `file`.
fn check_weights(path: &Path, got: u64, mut file: impl Read) -> Result<(), DownloadError> {
    let io = |source| DownloadError::Io { path: path.to_path_buf(), source };
    if got != WEIGHTS_SIZE {
        return Err(DownloadError::Size { path: path.to_path_buf(), got, want: WEIGHTS_SIZE });
    }
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let got = hash.finish_hex();
    if got != WEIGHTS_SHA256 {
        return Err(DownloadError::Checksum { path: path.to_path_buf(), got, want: WEIGHTS_SHA256 });
    }
    Ok(())
}

/// Download the weights into `models_dir` (as `dac_24khz/model.safetensors`) with
/// `curl` (or `wget`), which show their own progress on stderr. The file is written
/// under a temporary name, verified ([`verify_weights`]) and then renamed, so an
/// interrupted download never leaves a broken weights file behind. Returns the path.
pub fn download_weights(models_dir: &Path) -> Result<PathBuf, DownloadError> {
    let path = weights_path(models_dir);
    let dir = path.parent().expect("weights path has a parent");
    std::fs::create_dir_all(dir).map_err(|source| DownloadError::Io { path: dir.to_path_buf(), source })?;
    let part = path.with_extension("safetensors.part");
    let _ = std::fs::remove_file(&part);
    let tools: [(&'static str, Vec<String>); 2] = [
        (
            "curl",
            vec!["--fail".into(), "--location".into(), "--retry".into(), "3".into(), "--progress-bar".into(), "--output".into()],
        ),
        ("wget", vec!["--tries=3".into(), "--output-document".into()]),
    ];
    let mut ran = None;
    for (tool, args) in tools {
        // `Command` runs the program found on PATH, with our stdin/stdout/stderr.
        match Command::new(tool).args(&args).arg(&part).arg(WEIGHTS_URL).status() {
            Ok(status) => {
                ran = Some((tool, status));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(DownloadError::Io { path: PathBuf::from(tool), source }),
        }
    }
    match ran {
        None => return Err(DownloadError::NoTool { url: WEIGHTS_URL, path }),
        Some((tool, status)) if !status.success() => {
            let _ = std::fs::remove_file(&part);
            return Err(DownloadError::Tool { tool, status: status.to_string(), url: WEIGHTS_URL });
        }
        Some(_) => {}
    }
    if let Err(e) = verify_weights(&part) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &path).map_err(|source| DownloadError::Io { path: path.clone(), source })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_paths() {
        let p = weights_path(Path::new("m"));
        assert_eq!(p, Path::new("m").join("dac_24khz").join("model.safetensors"));
        // Without the environment variable: the executable's directory and its parents.
        if std::env::var_os(MODELS_ENV).is_none() {
            let c = candidate_paths();
            assert_eq!(c.len(), PARENT_LEVELS + 1);
            let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
            assert_eq!(c[0], weights_path(&exe_dir.join("models")));
        }
        let e = WeightsNotFound { searched: vec![PathBuf::from("a"), PathBuf::from("b")] }.to_string();
        assert!(e.contains("decdrm models download dac") && e.contains(MODELS_ENV), "{e}");
    }

    /// The downloaded weights at the workspace root (if present) are intact.
    #[test]
    fn workspace_weights_verify() {
        let path = weights_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models"));
        if !path.is_file() {
            eprintln!("skipped: {} not found", path.display());
            return;
        }
        verify_weights(&path).unwrap();
    }

    /// Built-in weights (`embed-weights` builds) are found last and verify.
    #[test]
    fn embedded_weights() {
        assert!(!is_embedded(Path::new("x")));
        match EMBEDDED_WEIGHTS {
            Some(_) => {
                assert!(is_embedded(Path::new(EMBEDDED_PATH)));
                assert!(find_weights().is_ok());
                verify_weights(Path::new(EMBEDDED_PATH)).unwrap();
            }
            None => assert!(!is_embedded(Path::new(EMBEDDED_PATH))),
        }
    }
}
