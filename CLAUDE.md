# DecDRM — notes for Claude

Rust DRM30 (Digital Radio Mondiale, modes A–D) receiver + transmitter. Read
`docs/DESIGN.md` first: it records the user's decisions, the architecture and the
milestone checklist (keep it current).

## Layout
- `crates/decdrm-core` — pure-Rust PHY/FEC/multiplex: `params`, `tables`, `cellmap`,
  `fec/` (CRC, dispersal, interleavers, conv code, Viterbi, QAM metrics, MLC),
  `interleave` (MSC cell interleaver), `fac`, `rx/` (receiver chain), `tx/`, `channel/`,
  `mux/` (SDC, service info, MSC demux, audio super frames, text messages), `dsp/`.
- `crates/decdrm-codecs` (+ `decdrm-fdk-sys`, `decdrm-opus-sys`, `decdrm-xaac-sys`) —
  FDK-AAC / libopus / libxaac (xHE-AAC encoder, patched at build time) FFI.
- `crates/decdrm-io` — WAV/FLAC, resampling, sound card (cpal), drift-compensated player.
- `crates/decdrm-data` — packet mode, MOT, Journaline, EPG, BWS, TPEG.
- `crates/decdrm-engine` — worker thread: source → `Session` (receiver + multiplex +
  audio/text/data pipelines) → audio out / data store; `Snapshot`s for the UIs.
- `crates/decdrm-station` — transmitter application layer (TOML station config).
- `apps/decdrm-cli` (binary `decdrm`: `rx`, `tx`, `devices`), `apps/decdrm-gui` (egui,
  MSRV 1.95 because of eframe).
- `third_party/` — pinned submodules (fdk-aac v2.0.3, opus v1.6.1, libxaac v0.1.13);
  clone with `--recursive`.
- `samples/` (user recordings) and `reference/` (Dream r1548 source, ETSI PDFs) are
  git-ignored and never committed. `reference/etsi/es_201980v040201p.txt` is the DRM
  spec as text — grep it for clause numbers and tables.

## Commands
- Build/test: `cargo test -p decdrm-core` (DSP is slow unoptimised; the dev profile
  uses opt-level 1, deps at 3).
- Full decode of a recording (audio, text, data):
  `cargo run --release -p decdrm-cli -- rx samples/DW_ModeB_10kHz.flac [--out a.wav]
  [--data-dir DIR] [--format iq]`; GUI: `cargo run --release -p decdrm-gui -- FILE`.
- Receiver-only smoke test:
  `cargo run --release -p decdrm-core --example rxfile -- samples/DW_ModeB_10kHz.flac`
  (append `iq` for I/Q files). `--example spectrum` prints a coarse spectrum,
  `--example modescores` the mode-detection margins per layout/SNR.
- Loopback suite incl. long sweeps:
  `cargo test --release -p decdrm-core --test loopback -- --include-ignored --nocapture`.

## Conventions
- DSP in `f64` (`Real`, `Cplx`); PCM audio is `f32`. Working sample rate 48 kHz.
- Bit streams are `Vec<u8>` with one bit per byte (`bits::BitReader/BitWriter`).
- Carrier index k ↔ FFT bin k mod N (true baseband, DC carrier at 0 Hz); carrier
  offset `c = k − Kmin` indexes per-symbol arrays.
- Timing shifts: `SymbolWindow::shift` > 0 means the window moved later; older channel
  values are rotated by e^{+j2πkΔ/N} to the newer timing. Dream's `iCurTimeCorr` is −shift.
- Cite ES 201 980 clauses and the Dream file an algorithm came from in doc comments.
- User is strong in DSP, newer to Rust: explain non-obvious Rust idioms briefly.
- Git: commit per milestone, repo-local identity already set; push to the private
  `origin` (github.com/CasualArclamp/DecDRM).
- Sub-agents share one target dir and a 31 GB machine: one cargo command at a time, no
  load generators, sound-card tests `#[ignore]`d, never play audio audibly.
