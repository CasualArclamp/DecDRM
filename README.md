# DecDRM

A Digital Radio Mondiale (DRM30) receiver — and, soon, transmitter — written in Rust.

DecDRM decodes DRM robustness modes A–D in every channel bandwidth (4.5–20 kHz) from
recordings or a live sound card (for example a virtual audio cable fed by a web SDR such
as a KiwiSDR). The signal processing is a spec-first reimplementation of ETSI ES 201 980
that borrows proven algorithms from [Dream](https://sourceforge.net/projects/drm/);
audio decoding uses Fraunhofer FDK-AAC (AAC, HE-AAC v1/v2, xHE-AAC) and libopus.

> **Status:** early development. The receiver synchronises to and decodes FAC, SDC and
> MSC on all in-scope test recordings (modes A/B/C, 9–20 kHz, real and I/Q inputs,
> inverted spectra, sound-card clock offsets up to 1250 ppm). Audio decoding, data
> services, the GUI and the transmitter are being integrated. See
> [docs/DESIGN.md](docs/DESIGN.md) for the plan and milestone status.

## Highlights

- Automatic frequency, robustness-mode, spectrum-occupancy and spectrum-inversion
  detection; no need to set the IF or flip the spectrum by hand.
- Sample-rate-offset acquisition from the pilot phase slope: sound-card clock errors of
  more than 1000 ppm are corrected before the first FAC is decoded.
- Wiener channel estimation in time and frequency with Doppler and delay-spread
  adaptation, impulse-response based timing and sample-rate tracking, iterative
  multilevel decoding.

## Building

```bash
git clone --recursive https://github.com/CasualArclamp/DecDRM.git
cd DecDRM
cargo build --release
```

Requirements: Rust 1.88 or newer, a C/C++ compiler (MSVC Build Tools on Windows, gcc on
Linux) and CMake for the vendored codecs, and on Linux the ALSA development package
(`libasound2-dev`).

## Usage

```bash
# decode a recording (real IF / audio input at any sample rate)
decdrm rx recording.flac

# I/Q recording
decdrm rx iq_recording.wav --format iq

# live from a sound-card input, e.g. a virtual audio cable
decdrm devices
decdrm rx --device "CABLE-A Output"
```

## Workspace

| Crate | Purpose |
|---|---|
| `decdrm-core` | DRM30 physical layer, FEC, receiver and transmitter chains (pure Rust) |
| `decdrm-codecs` | FDK-AAC / libopus wrappers for DRM audio framing |
| `decdrm-data` | Packet mode, MOT slideshow/website/EPG, Journaline, TPEG capture |
| `decdrm-io` | WAV/FLAC, resampling, sound-card input/output, drift-compensated playback |
| `decdrm-engine` | Threads, sources, decoding pipelines, status snapshots |
| `decdrm-cli` | The `decdrm` command-line tool |

## Licence

GPL-2.0-or-later (it derives from Dream, which is GPL). The vendored FDK-AAC library has
its own licence; see `third_party/fdk-aac/NOTICE`. AAC and DRM technologies are covered
by patents licensed through Via LA.
