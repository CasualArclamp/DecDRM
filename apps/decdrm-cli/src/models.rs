//! `decdrm models` — the weights of the neural codec (EnCodec).
//!
//! `decdrm models download encodec [--dir DIR]` fetches Meta's `facebook/encodec_24khz`
//! weights (93 MB, a pinned revision) with curl or wget, checks their SHA-256 and
//! installs them as `DIR/encodec_24khz/model.safetensors`. The default directory is
//! `$DECDRM_MODELS`, else `models` next to this executable. The receiver and
//! transmitter look there (and, for development builds, in a `models` directory up to
//! four levels above the executable, e.g. the workspace root). `decdrm models list`
//! shows where they are looked for and what is installed.

use anyhow::{Context, Result};
use decdrm_encodec::weights;
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    cmd: ModelsCmd,
}

#[derive(clap::Subcommand)]
enum ModelsCmd {
    /// Download and verify a model's weights (once per installation).
    Download {
        /// The model.
        #[arg(value_enum)]
        model: Model,
        /// Models directory (default: $DECDRM_MODELS, else `models` next to the
        /// executable).
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Download again even if intact weights are already there.
        #[arg(long)]
        force: bool,
    },
    /// Show where the weights are looked for and whether they are installed.
    List,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Model {
    /// Meta's EnCodec 24 kHz (facebook/encodec_24khz, 93 MB).
    Encodec,
}

pub fn run(a: ModelsArgs) -> Result<()> {
    match a.cmd {
        ModelsCmd::Download { model: Model::Encodec, dir, force } => download(dir, force),
        ModelsCmd::List => list(),
    }
}

fn download(dir: Option<PathBuf>, force: bool) -> Result<()> {
    let dir = match dir {
        Some(d) => d,
        None => weights::default_models_dir().context("cannot tell where the executable is; give --dir")?,
    };
    let path = weights::weights_path(&dir);
    if path.is_file() && !force {
        match weights::verify_weights(&path) {
            Ok(()) => {
                println!("{} is installed and intact (--force downloads it again)", path.display());
                return lookup_note(&path);
            }
            Err(e) => println!("{e}; downloading again"),
        }
    }
    println!(
        "downloading the EnCodec 24 kHz weights ({:.0} MB) from {}",
        weights::WEIGHTS_SIZE as f64 / 1e6,
        weights::WEIGHTS_URL
    );
    let path = weights::download_weights(&dir)?;
    println!("installed {} (SHA-256 verified)", path.display());
    lookup_note(&path)
}

/// Say so when the receiver and transmitter would not use the weights at `path`.
fn lookup_note(path: &Path) -> Result<()> {
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    match weights::find_weights() {
        Ok(found) if same(&found, path) => {}
        Ok(found) => println!(
            "note: DecDRM finds {} first; set {}={} to use these weights",
            found.display(),
            weights::MODELS_ENV,
            models_dir_of(path).display()
        ),
        Err(_) => println!(
            "note: DecDRM does not look there by itself; set {}={}",
            weights::MODELS_ENV,
            models_dir_of(path).display()
        ),
    }
    if !decdrm_encodec::BUILT_IN {
        println!("note: this decdrm is built without EnCodec; build it with `--features encodec` to use the codec");
    }
    Ok(())
}

/// The models directory a weights file is in (`<dir>/encodec_24khz/model.safetensors`).
fn models_dir_of(path: &Path) -> PathBuf {
    path.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default()
}

fn list() -> Result<()> {
    println!(
        "EnCodec 24 kHz (facebook/encodec_24khz): codec {}",
        if decdrm_encodec::BUILT_IN { "built in" } else { "not built in (build with `--features encodec`)" }
    );
    match std::env::var_os(weights::MODELS_ENV) {
        Some(v) => println!("{} = {}", weights::MODELS_ENV, v.to_string_lossy()),
        None => println!("{} is not set", weights::MODELS_ENV),
    }
    println!("weights looked for in:");
    for p in weights::candidate_paths() {
        println!("  {} {}", if p.is_file() { "found  " } else { "       " }, p.display());
    }
    match weights::find_weights() {
        Ok(p) => println!("using {}", p.display()),
        Err(_) => println!("not installed: run `decdrm models download encodec`"),
    }
    Ok(())
}
