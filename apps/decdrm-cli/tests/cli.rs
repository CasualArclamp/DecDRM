//! End-to-end tests of the `decdrm` binary: transmit the example station to a file,
//! decode it again, and check the command-line behaviour around it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn decdrm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_decdrm")).args(args).output().expect("run decdrm")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// A fresh directory with the example station files.
fn workdir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/decdrm-station/examples");
    for f in ["station.toml", "journaline.toml"] {
        std::fs::copy(examples.join(f), dir.join(f)).unwrap();
    }
    dir
}

#[test]
fn transmit_then_receive() {
    let dir = workdir("cli_tx_rx");
    let station = dir.join("station.toml");
    let wav = dir.join("drm.wav");
    let log = dir.join("rx.csv");
    let data = dir.join("data");

    let o = decdrm(&["tx", station.to_str().unwrap(), "--duration", "12", "--output", wav.to_str().unwrap()]);
    assert!(o.status.success(), "tx failed:\n{}", text(&o));
    // 12 s of 48 kHz mono 16-bit audio (30 frames of 400 ms) plus the header.
    let len = std::fs::metadata(&wav).unwrap().len();
    assert!(len >= 12 * 48_000 * 2, "WAV only {len} bytes");

    let o = decdrm(&[
        "rx",
        wav.to_str().unwrap(),
        "--status-every",
        "0",
        "--log",
        log.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    let out = text(&o);
    assert!(o.status.success(), "rx failed:\n{out}");
    assert!(out.contains("label \"DecDRM Radio\""), "no label:\n{out}");
    assert!(out.contains("text: "), "no text message:\n{out}");
    let audio_ok: u64 = out
        .lines()
        .find(|l| l.starts_with("MSC frames"))
        .and_then(|l| l.split(": ").nth(1))
        .and_then(|s| s.split(" frames ok").next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    assert!(audio_ok >= 30, "only {audio_ok} audio frames decoded:\n{out}");

    let csv = std::fs::read_to_string(&log).unwrap();
    let rows: Vec<&str> = csv.lines().collect();
    assert!(rows[0].starts_with("utc,signal_s,state"), "header: {}", rows[0]);
    assert!(rows.len() >= 8, "only {} log lines", rows.len());
    assert!(rows.last().unwrap().contains(",Locked,B,"), "last row: {}", rows.last().unwrap());
    // The station's EPG arrives as a data object.
    assert!(data.join("epg").exists(), "no EPG saved:\n{out}");
}

#[test]
fn transmit_without_end_is_refused_before_creating_the_file() {
    let dir = workdir("cli_tx_endless");
    let wav = dir.join("endless.wav");
    let o = decdrm(&["tx", dir.join("station.toml").to_str().unwrap(), "--output", wav.to_str().unwrap()]);
    assert!(!o.status.success(), "an endless file transmission must be refused");
    assert!(text(&o).contains("--duration"), "{}", text(&o));
    assert!(!wav.exists(), "no output file may be left behind");
}

#[test]
fn check_prints_the_multiplex() {
    let dir = workdir("cli_tx_check");
    let o = decdrm(&["tx", dir.join("station.toml").to_str().unwrap(), "--check"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("mode B") && out.contains("DecDRM Radio"), "{out}");
}

#[test]
fn bad_arguments_fail_with_usage() {
    let o = decdrm(&["rx"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("--device"), "{}", text(&o));
}
