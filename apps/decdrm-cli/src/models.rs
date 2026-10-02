//! `decdrm models` — the weights of the neural codec (DAC).
//!
//! `decdrm models download dac [--dir DIR]` fetches Descript's `descript/dac_24khz`
//! weights (299 MB, a pinned revision) with curl or wget, checks their SHA-256 and
//! installs them as `DIR/dac_24khz/model.safetensors`. The default directory is
//! `$DECDRM_MODELS`, else `models` next to this executable. The receiver and
//! transmitter look there (and, for development builds, in a `models` directory up to
//! four levels above the executable, e.g. the workspace root). `decdrm models list`
//! shows where they are looked for and what is installed.

use anyhow::{Context, Result};
use decdrm_dac::weights;
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
    /// The Descript Audio Codec, 24 kHz (descript/dac_24khz, 299 MB).
    Dac,
}

pub fn run(a: ModelsArgs) -> Result<()> {
    match a.cmd {
        ModelsCmd::Download { model: Model::Dac, dir, force } => download(dir, force),
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
        "downloading the DAC 24 kHz weights ({:.0} MB) from {}",
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
    if !decdrm_dac::BUILT_IN {
        println!("note: this decdrm is built without DAC; build it with `--features dac` to use the codec");
    }
    Ok(())
}

/// The models directory a weights file is in (`<dir>/dac_24khz/model.safetensors`).
fn models_dir_of(path: &Path) -> PathBuf {
    path.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default()
}

fn list() -> Result<()> {
    println!(
        "DAC 24 kHz (descript/dac_24khz): codec {}",
        if decdrm_dac::BUILT_IN { "built in" } else { "not built in (build with `--features dac`)" }
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
        Err(_) => println!("not installed: run `decdrm models download dac`"),
    }
    Ok(())
}
