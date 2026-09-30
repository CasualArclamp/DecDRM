# DecDRM — design

DecDRM is a Digital Radio Mondiale (DRM30) receiver **and** transmitter written in Rust.
This document records the decisions from the kickoff Q&A (2026-09-29), the architecture,
and the milestone plan. Keep the milestone checklist current.

## Decisions

| Topic | Decision |
|---|---|
| Standard scope | DRM30 robustness modes **A, B, C, D**; all spectrum occupancies (0–5, 4.5–20 kHz); 4/16/64-QAM; SM, HMsym, HMmix; short & long interleaving; all protection levels. No DRM+ (mode E), no analog AM/SSB/FM, no AMSS. |
| Purpose | Personal / hobby. |
| Relationship to Dream | Spec-first idiomatic Rust, freely translating Dream's proven algorithms (sync, Wiener channel estimation, MLC decoding). Licence therefore **GPL-2.0-or-later**. Reference: Dream `branches/dream-mjf` r1548. |
| Receiver performance | Match or beat Dream on weak/fading signals (Wiener channel estimation, soft-decision Viterbi, iterative MLC, sample-rate-offset tracking). |
| Audio decoding | Behind a Rust codec interface. AAC / HE-AAC v1/v2 and **xHE-AAC** via **FDK-AAC** (vendored, built from source, statically linked). **Opus** (Dream's extension) via libopus. CELP/HVXC: detected and reported as unsupported. |
| Data services | Text messages, Journaline, MOT Slideshow, EPG, Broadcast Website, TPEG/unknown (raw data saved). Broadcast clock and alternative-frequency (AFS) info from the SDC. |
| Inputs | Recorded files (WAV/FLAC; real IF or I/Q; any sample rate) and live sound card (including a virtual audio cable fed by web SDRs such as KiwiSDR). No direct SDR drivers, no network clients. |
| Outputs | Live audio (with clock-drift compensation) and logs/metrics (CSV/JSON). |
| Transmitter | Full transmitter: AAC/HE-AAC (FDK encoder), xHE-AAC (libxaac encoder), Opus, plus **EnCodec** (Meta's neural codec, via candle) as an experimental DecDRM-only extension. Carries every data service the receiver decodes. Output to WAV/FLAC file and sound card. Includes a channel simulator (spec channel models) for loopback testing. |
| UI | Library crates + CLI + desktop GUI (**egui**). The GUI has RX/TX modes like Dream; the transmitter is also scriptable from the CLI with a TOML config. |
| Platforms | Windows x86_64 (primary), Linux x86_64. |
| Workflow | Max autonomy; commit per milestone; local git + private GitHub repo `CasualArclamp/DecDRM`. |
| First milestone | Audio from a real recording, end to end, via the CLI. |

## Architecture

```
            ┌────────────── apps ──────────────┐
            │ decdrm (CLI)      decdrm-gui (egui)│
            └──────┬───────────────────┬───────┘
                   │    decdrm-engine   │   threads, device/file plumbing,
                   │ (runtime wiring)   │   codec + data-decoder registry
      ┌────────────┼──────────┬─────────┴───────┬────────────────┐
      │            │          │                 │                │
 decdrm-core   decdrm-codecs  decdrm-data    decdrm-io     (future) decdrm-xaac,
 PHY+FEC+mux   FDK-AAC, Opus  MOT/Journaline WAV/FLAC/cpal  decdrm-encodec
 (pure Rust)   (FFI)          EPG/BWS/TPEG   resampling
      ▲            ▲
      │      decdrm-fdk-sys, decdrm-opus-sys (vendored C in third_party/)
```

* **decdrm-core** (pure Rust, no I/O, no C): mode/occupancy tables, OFDM cell map and
  pilots, receiver chain (input conditioning → frequency acquisition → time sync → OFDM
  demodulation → pilot-based sync → channel estimation → cell demapping → FAC/SDC/MSC
  decoding), FEC (energy dispersal, convolutional code, puncturing, soft Viterbi, bit and
  cell interleaving, MLC), FAC/SDC parsing, MSC demultiplexing, audio super-frame parsing,
  text messages, and the transmitter chain (the exact inverse) plus the channel simulator.
  DSP runs in `f64` (`Complex<f64>`); PCM audio is `f32`.
* **decdrm-codecs**: safe wrappers over FDK-AAC (DRM transport `TT_DRM`, configured with
  the raw SDC type-9 bytes, as Dream does) and libopus.
* **decdrm-data**: packet-mode reassembly, MSC data groups, MOT (slideshow, broadcast
  website, EPG objects), Journaline, EPG binary→XML, TPEG/raw capture — decode and encode.
* **decdrm-io**: WAV/FLAC reading and writing, resampling to the 48 kHz working rate,
  sound-card input/output via cpal.
* **decdrm-engine**: glues the above together on threads (input → receiver → audio out),
  owns the codec/data registries, publishes status snapshots for the UIs, writes logs.
* **decdrm-station**: the transmitter application layer: a TOML station config
  (services, codecs, data applications, output) → planned multiplex → frames from
  `decdrm-core::tx` → WAV/FLAC or sound card.

### Receiver data flow

The receiver is push-driven and single-threaded internally: `Receiver::push(&[samples])`
runs every stage for which enough input is buffered and emits `ReceiverEvent`s (status,
FAC/SDC updates, audio super-frames, data-stream frames, text messages). Stages keep their
own buffers; timing/frequency corrections flow backwards through explicit feedback fields
rather than shared global state (Dream's `CParameter` is split into typed per-stage state).

Working sample rate is **48 kHz** (Dream's `SOUNDCRD_SAMPLE_RATE`), at which the useful
symbol lengths are 1152/1024/704/448 samples for modes A/B/C/D.

## Milestones

- [x] **M0 Setup** — workspace, references, design doc, git + private GitHub.
- [x] **M1 RX spine** — file input → sync → OFDM → channel estimation → FAC/SDC/MSC →
      AAC audio (FDK) → WAV + playback, via CLI, on the mode A/B 10 kHz samples.
      *Done:* audio on every DRM recording in `samples/` (21 of 22; the 22nd is AM/AMSS).
- [x] **M2 RX completeness** — all modes A–D and occupancies 0–5, HMsym/HMmix, long
      interleaving, flipped spectrum, I/Q inputs, offsets; Wiener channel estimation,
      iterative MLC, SRO tracking; minimal modulator + channel simulator for modes/SOs we
      have no recordings of (mode D, 4.5/5/18 kHz).
      *Done:* `tests/loopback.rs` covers every layout (I/Q, real IF, offset), MSC
      bit-exact for 16/64-QAM SM, HMsym, HMmix with short/long interleaving, AWGN and
      channels 1–6, ±50 ppm / frequency offsets. Beyond Dream: sub-bin SRO estimation,
      two-window mode detection, pilot-slope SRO acquisition, seamless occupancy change.
      SRO estimation: translation of PDS snapshots (phase-slope fraction with a
      robust fallback), exact pilot-grid scaling, lag-compensated tracking;
      pilot-slope acquisition only beyond the ~1000 ppm unaided lock range and only
      when confirmed. Timing jumps (samples lost/inserted by network streams) are
      detected from the cyclic-prefix correlation and a time-pilot monitor and
      resynchronised in ~1 s without a full restart (`examples/dropout`).
      Sensitivity (`examples/bercurve.rs`, BER 1e-4 after decoding, 64-QAM R = 0.6,
      2 MLC iterations, real synchronisation and channel estimation, modes A for ch1-2
      and B for ch3-5) against ES 201 980 annex A (ideal estimation): ch1 14.8 dB
      (14.9), ch2 15.8 (16.5), ch3 23.8 (23.2), ch4 23.8 (22.3), ch5 22.0 (20.4) — at
      most 1.6 dB from the ideal-receiver figures (pooled over 3-4 seeds × 300-600 s per
      point, 0.1 dB grid for ch1-2; earlier 60 s runs had ch4 1.5 dB too optimistic).
      Soft metric (2026-09-30, same runs): Dream's |r/h − s|·|h| against the Euclidean
      |r/h − s|²·|h|² (`MetricKind`): Dream's is 0.1-0.35 dB better at BER 1e-4 on every
      channel; the Euclidean curve is steeper and has the lower error floor on ch1-2,
      so at MSC frame error rate 1 % it wins there by ~0.3 dB but loses by ~0.1 dB on
      ch3 and ch5. The default stays Dream's metric.
- [x] **M3 Codecs & text** — xHE-AAC, Opus, text messages, concealment, drift-compensated
      live playback. *Done* (xHE-AAC on FMGold, Opus in all three signalling variants).
- [x] **M4 Data services** — Journaline, MOT Slideshow, BWS, EPG, TPEG/raw; clock & AFS.
      *Done:* decoders verified on recordings (slideshow, Journaline, BWS) and with our
      transmitter (EPG, TPEG, raw applications, alternative frequencies — there are no
      recordings of those here). TPEG and uninterpreted applications are captured to
      `<data dir>/raw/` (data fields of the CRC-checked data groups; stream-mode bytes);
      the station sends them from a file (`type = "tpeg"` / `"raw"` with `app_id`).
      Clock and AFS (SDC types 3, 4, 7, 11; the station's `[afs]` section) shown in the
      CLI and the GUI (`Snapshot::afs`, `time`).
- [x] **M5 Live input & logging** — sound-card input (VAC), CSV/JSON logs & metrics.
      *Done:* sound-card input (CLI `--device`, GUI); `decdrm rx --log FILE.csv|.jsonl`
      writes metrics rows (and events in JSON Lines).
- [x] **M6 GUI** — egui: spectrum/waterfall, constellations, SNR/MER, sync status,
      service list, text, slideshow, Journaline browser, EPG, clock/AFS.
      *Done:* receiver tab (spectrum, waterfall, constellations, decoded-audio
      spectrum, channel, impulse response, SNR per carrier, history of the last five
      minutes of signal with error rates per 10 s, LEDs, Dream-style service bars (codec,
      SBR/PS, rates, bit rate, EEP/UEP, text, data applications, CA), text, slideshow,
      Journaline, broadcast website (opens the HTML start page in the system browser on
      a click), EPG, broadcast clock with local time, alternative frequencies, data
      directory) and transmitter tab (TOML editor with error locations, validation,
      transmit to file/sound card, status, TX spectrum).
- [x] **M7 Transmitter** — full TX chain, FDK AAC/HE-AAC encoding, all data services,
      file/sound-card output, channel simulator, loopback BER tests; GUI TX tab + CLI.
      *Status:* `decdrm-station` + `decdrm tx station.toml` + GUI tab done
      (AAC/HE-AAC/v2 up to AAC's bit-rate limit, Opus, slideshow, website, Journaline,
      EPG, text, time; all MSC modes incl. HM/UEP), verified by loopback through our
      receiver, also live through a virtual audio cable. A sound-card input with a
      sound-card output is drift-compensated: a PI loop trims the input resampler to
      hold the capture backlog at 0.3 s (primed by the output queue at start-up); live
      85 s run: trim within ±160 ppm, 0 underruns.
- [x] **M8 TX codecs** — xHE-AAC (libxaac encoder), Opus.
      *Done:* station codecs `xhe-aac` (24/32/48 kHz by stream rate, mono/stereo,
      text) and `opus`; `XheAacFramer` writes the header reservoir level itself and
      refuses to pad mid-frame. Loopbacks mode B/A/D decode with 0 concealed frames.
      Not supported: MPS212 (4:1 stereo), 38.4 kHz stereo.
- [x] **M9 EnCodec** — experimental neural-codec extension (TX + RX).
      *Done:* `decdrm-encodec` (feature `encodec`, candle 0.9, own streaming SEANet
      identical to candle-transformers' EnCodec), signalled as SDC type 9 audio coding
      10 with an `ENC1` config (Dream-safe), 1.5–24 kbit/s tiers, per-region CRC-8,
      optional repetition of the base layers, latent interpolation for lost frames.
      At 15 dB (mode B, 64-QAM) 2.1 % of frames concealed against 26.5 % for HE-AAC.
- [x] **M10 Polish** — performance, Linux verification, docs.
      *Done:* performance measured, no hot spot worth optimising: the receiver decodes
      10 kHz signals at 100–150× real time and 20 kHz ones at ~45× on one core;
      EnCodec encodes/decodes at ~15× real time; the GUI needs 2–6 % of a core while
      decoding live (~12 frames/s, < 1 ms per frame; a frame per input event while the
      mouse moves). Linux: the manual GitHub Actions workflow builds, tests and lints
      the workspace, then smoke-tests device listing without a sound card, a CLI
      transmit → receive round trip and the GUI under Xvfb/Mesa (screenshot artifact).
      Docs: `docs/USER_GUIDE.md`; rustdoc builds without warnings. The station's
      `[simulate]` section (and `decdrm tx --channel-model/--snr`) exposes the channel
      simulator as a test-signal generator; its clock error is applied to the output
      samples, so it scales the IF like a real sound-card clock.

## Conventions

* Cite the spec in doc comments: `ES 201 980 §7.3.2`. When an algorithm is borrowed from
  Dream, name the Dream file/class it came from.
* No `unsafe` outside the `*-sys` crates and the thin FFI wrappers in `decdrm-codecs`.
* Tests: unit tests per module; integration tests decode files from `samples/` and are
  skipped (not failed) when the samples are absent; loopback tests use our transmitter.
* `samples/` (user-provided recordings) and `reference/` (Dream source, specs) are not
  committed.

## Reference material

* Dream source r1548 (`branches/dream-mjf`) — `reference/dream-mjf/` (fetched over SVN HTTP
  from `https://svn.code.sf.net/p/drm/code/branches/dream-mjf/`).
* ETSI specifications (free; store them in `reference/etsi/`):
  * [ES 201 980 V4.2.1](https://www.etsi.org/deliver/etsi_es/201900_201999/201980/04.02.01_60/es_201980v040201p.pdf) — DRM system specification
    (`reference/etsi/es_201980v040201p.pdf`, plus a `.txt` extraction for grepping)
  * TS 101 968 — DRM data applications directory
  * TS 102 979 — Journaline
  * EN 301 234 — MOT protocol; TS 101 499 — MOT SlideShow; TS 101 498 — Broadcast Website
  * TS 102 818 / TS 102 371 — EPG (XML and binary encoding)
