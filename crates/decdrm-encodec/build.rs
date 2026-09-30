//! With the `embed-weights` feature: find the EnCodec weights to build into the
//! executable (`$DECDRM_MODELS`, else the workspace's `models` directory) and hand their
//! path to `include_bytes!` as `DECDRM_EMBEDDED_WEIGHTS`.

use std::path::PathBuf;

/// Size of the weights file (`weights::WEIGHTS_SIZE`; the build script cannot use the
/// crate itself). The SHA-256 is checked when the model is verified at run time.
const WEIGHTS_SIZE: u64 = 93_119_608;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DECDRM_MODELS");
    if std::env::var_os("CARGO_FEATURE_EMBED_WEIGHTS").is_none() {
        return;
    }
    let models = std::env::var_os("DECDRM_MODELS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("../../models"));
    let path = models.join("encodec_24khz").join("model.safetensors");
    match std::fs::metadata(&path) {
        Ok(m) if m.len() == WEIGHTS_SIZE => {}
        Ok(m) => panic!(
            "{} has {} bytes, the EnCodec weights have {WEIGHTS_SIZE}; run `decdrm models download encodec --force`",
            path.display(),
            m.len()
        ),
        Err(e) => panic!(
            "the `embed-weights` feature needs the EnCodec weights at {} ({e}); run `decdrm models download encodec` first",
            path.display()
        ),
    }
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rustc-env=DECDRM_EMBEDDED_WEIGHTS={}", path.display());
}
