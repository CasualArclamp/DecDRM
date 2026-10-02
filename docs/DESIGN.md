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
| Audio decoding | Behind a Rust codec interface. AAC / HE-AAC v1/v2 and **xHE-AAC** via **FDK-AAC** (vendored, built from source, statically linked). **Opus** (Dream's extension) via libopus. CELP/HVXC: detected and reported as unsupported. **EVS** sent as data by KCBS (6140 kHz): recognised and shown, not decoded. An optional build of the 3GPP reference decoder (feature `evs`, from a user-supplied source zip) was removed on 2026-10-01 so the repository can be public; no 3GPP code goes into DecDRM. |
| Data services | Text messages, Journaline, MOT Slideshow, EPG, Broadcast Website, TPEG/unknown (raw data saved). Broadcast clock and alternative-frequency (AFS) info from the SDC. |
| Inputs | Recorded files (WAV/FLAC; real IF or I/Q; any sample rate), live sound card (including a virtual audio cable fed by any web SDR), and KiwiSDRs directly over the network (their I/Q, retunable while connected; added 2026-10-01 at the user's request). No direct SDR drivers. The network clients are the KiwiSDR client and the transmitter's web stream audio input. |
| Outputs | Live audio (with clock-drift compensation) and logs/metrics (CSV/JSON). |
| Transmitter | Full transmitter: AAC/HE-AAC (FDK encoder), xHE-AAC (libxaac encoder), Opus, plus **EnCodec** (Meta's neural codec, via candle) as an experimental DecDRM-only extension. Carries every data service the receiver decodes. Programme audio from a file, a sound card, a test tone or an internet radio stream (Icecast/SHOUTCAST over HTTP/HTTPS, its titles as text messages). Output to WAV/FLAC file and sound card. Includes a channel simulator (spec channel models) for loopback testing. |
| UI | Library crates + CLI + desktop GUI (**egui**). The GUI has RX/TX modes like Dream; the transmitter is also scriptable from the CLI with a TOML config. |
| Platforms | Windows x86_64 (primary), Linux x86_64. |
| Workflow | Max autonomy; commit per milestone; local git + GitHub repo `CasualArclamp/DecDRM`, private until 2026-10-01 and public since (so: no recordings, reference material, model weights, secrets or 3GPP code in it). |
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
  the raw SDC type-9 bytes, as Dream does; ADTS for web streams) and libopus.
* **decdrm-data**: packet-mode reassembly, MSC data groups, MOT (slideshow, broadcast
  website, EPG objects), Journaline, EPG binary→XML, TPEG/raw capture — decode and encode.
* **decdrm-io**: WAV/FLAC reading and writing, resampling to the 48 kHz working rate,
  sound-card input/output via cpal.
* **decdrm-engine**: glues the above together on threads (input → receiver → audio out),
  owns the codec/data registries, publishes status snapshots for the UIs, writes logs.
* **decdrm-station**: the transmitter application layer: a TOML station config
  (services, codecs, data applications, output) → planned multiplex → frames from
  `decdrm-core::tx` → WAV/FLAC or sound card. Audio inputs: file, sound card, test
  tone, web stream (`webstream`: its own HTTP/1.1 client with rustls, ICY metadata,
  playlists, MP3/ADTS framing and an Ogg demuxer; decoding by symphonia, FDK-AAC and
  libopus).
* **decdrm-schedule** (pure Rust, no DSP): broadcast schedules for "what is on the
  air now" — Dream's `DRMSchedule.ini` and EiBi's CSV, on-air logic, sources and
  downloads (curl/wget, on request only); used by `decdrm schedule` and the GUI's
  *Schedule* tab.

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
      Soft metric (2026-09-30, same runs, paired seeds; `MetricKind`): Dream's
      |r/h − s|·|h| against the Euclidean |r/h − s|²·|h|²: Dream's is 0.1-0.35 dB
      better at BER 1e-4 on every channel; the Euclidean curve is steeper with the
      lower floor on ch1-2 (better at MSC frame error rate 1 % there, ~0.1 dB worse on
      ch3 and ch5). A Huber shape (squared up to δ, linear beyond; δ = c × the level's
      subset half-distance, so only abnormally large distances — from wrong decisions
      of other MLC levels — are clipped) recovers Dream's waterfall on AWGN but, with
      |h|² weighting, loses 0.2-0.46 dB on the fading channels: the weighting is what
      Dream gets right (|h|² over-trusts strong carriers there). With Dream's |h|
      weighting (`HuberAmplitude`), c = 0.25 is never worse than Dream at BER 1e-4
      (−0.01 to −0.07 dB) and 0.17 / 0.11 dB better at FER 1 % on ch1 / ch2 (neutral
      within ±0.05 dB on ch3-5; 16-QAM checked on ch1/ch3): now the MSC default
      (`rx::MSC_METRIC`); FAC and SDC keep Dream's metric. The recordings decode
      identically (their errors are not SNR-limited).
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
      a click), EPG, broadcast clock with local time, alternative frequencies, playback
      volume, data
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
- [x] **KCBS EVS** (2026-10-01) — Korean Central Broadcasting on 6140 kHz signals its
      only service as data (user application 0x000) but sends 3GPP EVS audio.
      *Done:* framing reverse-engineered from recordings (`decdrm_evs::kcbs`: one
      716-byte data group per 400 ms with 20 EVS frames of 264 bits = 13.2 kbit/s,
      super-wideband ACELP, the first byte of every fourth frame moved to the front; a
      52-byte side channel with a 1.2 s counter). Confirmed with the 3GPP decoder
      against a random-payload control (voiced pitch strength 0.56 vs 0.27, spectral
      flux 7.8 vs 12 dB). `decdrm-evs` builds the TS 26.443 float decoder from the zip
      in `reference/evs/` (feature `decoder`; engine/CLI/GUI feature `evs`) behind a
      frame-by-frame shim that matches the reference program to 1 LSB. The engine
      locks a data channel as EVS after two matching data groups (all 20 frames
      signalling one bandwidth; random data ~10⁻⁸), plays it like an audio service
      (lost data groups concealed) and shows it as EVS in the service bars.
      *Then:* the decode sounded glitchy because the station's encoder departs from
      EVS in INACTIVE, TRANSITION and LR-MDCT frames (the decoder's bit-error checks
      fire on 34 %/42 % of the INACTIVE/MDCT frames, 0 % for the 3GPP encoder; TRANSITION
      frames clip; EVS 12.0–12.2 decoders do no better). `KcbsDecoder` conceals those
      types, a burst guard (one frame of delay; on a clipping or +15 dB frame a fresh
      decoder replays 0.5 s and conceals it and its predecessor) removes the rest (0
      clipped frames on the recordings, from ~47 per 32 s), and concealed pauses get
      comfort noise: spectrum and level measured from the pause frames a second decoder
      decodes normally (outliers rejected; −61 dBFS, hum-weighted), 129-tap FIR-shaped
      white noise 3 dB below it, 10 ms fades.
      *Removed* (2026-10-01, at the user's request, so the repository can be public):
      the decoder build (feature, shim, `KcbsDecoder`, burst guard, comfort noise) and
      its playback in the engine. Recognition stays: the framing, the signalling
      table, the service shown as EVS with "no decoder" and "likely encrypted", and the
      findings above, recorded in `decdrm_evs::kcbs`.
- [x] **Station schedule** (2026-10-01) — which DRM stations are on the air now, like
      Dream's Stations dialog (`StationsDlg.cpp`, `Schedule.cpp`), to know what to
      tune a web SDR to.
      *Done:* `decdrm-schedule` reads Dream's `DRMSchedule.ini` as `ReadINIFile` does
      (Sunday-first day flags, `0000000` = irregular = every day; bad records skipped
      instead of ending the file) and EiBi's seasonal CSV as `ReadCSVFile` does, with
      Dream's code tables (`TableStations.cpp`, generated into `eibi_tables.rs`) for
      languages, targets, countries and sites; columns found by header name, lenient
      days (`Mo-Fr`, `Sa,Su`, `SaSu`, `1245`, `Fr-Mo`, keywords) and validity dates
      (`ddmm`, as EiBi's `README.TXT` says and the A26 file confirms; a `[mmyy]` after
      the stop date is the month last heard, shown as a note); DRM = EiBi's mark
      `DIGITAL` after the station name (`BBC DIGITAL`; not in the README, and the A26
      file has no "DRM" at all — the first version, looking for that word, found 0 of
      its 118 DRM entries), or the word "DRM" in station, remarks or language. On
      air: the start day decides (Dream misses Friday's 2300-0100 on Saturday at
      00:30), `start == stop` is all day, a single annual validity date is taken within
      the broadcast season and two make a window that recurs every year (compared by
      month and day, so a permanent entry's `05-15`…`12-20` holds in either season's
      file), Dream's ending-soon (10 min) and
      starting-soon (15 min) states. UTC calendar from `SystemTime` (civil-from-days),
      seasons A/B from the last Sundays of March/October (`sked-a26.csv`). Sources
      (EiBi's current season file, Dream's DRMDX URL on baseportal.com) configurable
      in `sources.toml` in the per-user `schedule` folder; downloads only on request,
      via curl/wget into a `.part` file that replaces the old copy only if it parses.
      CLI `decdrm schedule` (`--update --all --at --freq --source --all-broadcasts
      --filter --sources --dir`); GUI *Schedule* tab (background load/update, rows
      cached per minute and painted with `show_rows`, the received frequency from the
      *Frequency* box or the recording's file name — KiwiSDR, HDSDR, SDR# naming —
      highlighted and named in the log at Start; a click copies the frequency).
      EiBi's README codes (`entry::Activity`, `MonthDays`): persistence code 4 = only
      in the winter (B) seasons, 5 = only in the summer (A) seasons, 8 = inactive (never
      on the air, like `alt`, an alternative frequency not usually in use), 90 + a code
      = utility station; a single date on codes 1–5 (copied into every season's file)
      is a note, not a bound; Days forms `1.Sa`/`2.Su` (the n-th weekday of the month),
      `1WeFr` (the first Wednesday and the Friday after it), `Last7`, `MF-15` (Monday to
      Friday up to the 15th), `15Sep` (that day only), `altFr` (every Friday, noted as
      alternate weeks). In the A26 file this takes the 359 winter-only entries (among
      them CNR1 DIGITAL on 15180 kHz and Scandinavian Weekend Radio's winter schedule)
      and the 80 inactive ones (SE-TA2 DIGITAL) off the air.
- [x] **Fading and delay–Doppler maps** (2026-10-01) — two displays Dream lacks.
      *Done:* `rx::scatter`: the last 6 s of channel estimates (one row per symbol,
      brought to the newest timing with each row's cumulative shift) → Hann-windowed
      inverse FFT over the carriers (delay, 2× zero padding) → Hann-windowed FFT over
      time per delay (Doppler, ≥ 4× zero padding), once a second (with every new symbol
      since the 60 Hz plots); dB below the strongest point, floor −40 dB, delays
      −Tg/4 … 1.25 Tg, Doppler ±5/8/10 Hz by mode. GUI tabs *Fading* (gain per carrier
      per snapshot, a minute, colours from the median gain; a row per symbol since) and
      *Delay–Doppler*. Checked against channel model 3 (four paths, spreads
      0.1–2 Hz, in place) and on KCBS (separate ionospheric paths). The waterfall also
      fits the DRM signal now, at the spectrum's full 2048-bin resolution.
- [x] **MDI / RSCI / DCP** (2026-10-02) — the user's choice of next feature: both
      input roles (the receiver ← MDI/RSCI, the transmitter ← MDI as a modulator),
      full RSCI status (Dream parity), GUI and RCI control (no CLI flags), the ETSI
      specs to be cross-checked once downloaded (TS 102 820, 821, 349).
      *Done:* new crate `decdrm-mdi`: AF packets, PFT (fragmentation, addressing,
      RS(255,207) shortened, GF poly 11D, roots α⁰…α⁴⁷; byte j of fragment i is byte
      j·F + i of the RS block; Karn's errors-and-erasures decoder, lost fragments as
      erasures), TAG items, MDI frames (`*ptr dlfc fac_ sdc_ sdci robm str0-3 info`),
      RSCI status (every item Dream reads or writes, `rpil` included), RCI commands
      (`cact cfre cdmo crec cpro cser`), recordings (TS 102 821 annex B file framing,
      raw AF/PFT, pcap and pcapng with IPv4 reassembly), UDP in Dream's origin syntax
      (multicast join on an interface, source filter in software; std only), dlfc
      repeat/loss logic as Dream's. Engine: `InputSpec::Mdi`, `Session::new_mdi`/
      `push_mdi` (FAC/SDC into the ensemble, the streams to the decoders via the
      demultiplexer-free `on_logical`), RSCI items as the receiver status (MER, WMER,
      FAC WMER, Doppler, delay, sync) and plots (`rpsd` at −7.875 kHz + 0.1875 kHz per
      value relative to the DC carrier, a waterfall row per frame; `rpir` as the PDS),
      tune and service selection to the RSCI receiver by RCI, `EngineConfig::
      rci_listen` (RCI commands → engine commands), snapshot `input.mdi`, `remote`.
      Station: the modulator (`[mdi]`: input, buffer_frames; alignment by the FAC frame
      identity, fillers for lost/late/damaged frames, dlfc for late ones, transmitter
      and output stage rebuilt on a channel change at a super frame start, a reserve
      and super-frame trimming with a sound card, services from the MDI's FAC/SDC in
      the status), `capture_mdi` (a frame as MDI) and the `mdi_source` example. GUI:
      *MDI/RSCI* source (address, recording picker, RCI address, frequency → RCI),
      MDI recordings via *Open…*, status strip (link, profile, signal, frequency,
      receiver name, details), ⚙ remote control; Transmitter tab *Modulator* form card
      and status card. Tests: crate units (RS limits, PFT with losses, recordings,
      UDP), end to end a station's MDI recording through the engine, RSCI over UDP
      with PFT/FEC and a lost fragment per packet plus RCI back, RCI remote control,
      the modulator from a recording (receiver decodes it with no audio lost) and from
      UDP with a lost frame filled.
- [x] **Live Journaline updates** (2026-10-02) — GitHub issue #1: the page file
      edited while transmitting reaches the air without a restart.
      *Done:* `JournalineEncoder::replace_all` (decdrm-data) makes the carousel carry
      a new page set atomically (everything encoded first): the same content keeps its
      revision, new content gets the next one (mod 8), pages that are gone stop;
      once the carousel is on the air, new and changed pages are sent next, ahead of
      the cycle (the cycle then goes on where it was). The station keeps a second
      handle to each Journaline carousel (`Arc<Mutex<_>>`, the packet multiplexer
      owns the boxed source) and a `JournalineWatch` per page file: checked once a
      second (modification time and length), loaded once unchanged for a check, so a
      file still being saved is not read; a page file that does not validate changes
      nothing and is reported in the log (once per change).
      `Station::reload_journaline()` loads at once; `AppStatus::journaline` (pages,
      updates, the last error). GUI: the Transmitter tab's services card shows them,
      with an *Update* button; CLI status lines show the pages and updates. Tests:
      carousel units (order, revisions, removal while queued, all or nothing), the
      watch with simulated time, a loopback in which the receiver sees the changed
      menu and page (revision 1) and the new page, the GUI worker's Update path.
- [x] **Audio recording in the GUI** (2026-10-02) — GitHub issue #2 (Dream's *Save
      audio as WAV*).
      *Done:* engine `Command::StartRecording(path)` / `StopRecording`; `AudioOut`'s
      recorder keeps the decoded format (WAV or FLAC by the name, 16-bit), opens at
      once when audio is coming (else with the first audio), carries on in `name-2.wav`
      … (never an existing file) when the format changes, checkpoints every 5 s
      (`FileWriter::flush`: hound rewrites the WAV sizes, so a crash loses at most 5
      s), and ends with its error (logged once) if a write fails; its state
      (`RecordingStatus`: files, seconds, format, active, error) is in the snapshot,
      and the last one stays after it ends. GUI: *Record…* / *Stop recording* under
      the volume (dialog with `<service> <date> <time> UTC.wav` in the last folder),
      the time and file while recording, *Saved … in …* with *Show* afterwards;
      `decdrm-gui --start --record FILE` for timed recordings with `--exit-after`. The
      CLI's `--out` uses the same recorder (numbered parts instead of a time stamp).
      Tests: recorder units (format changes, existing files, errors, waiting for
      audio), the WAV checkpoint read while still being written, the commands end to
      end on a station signal received in real time (the 1 kHz tone in the file).
- [x] **Diversity reception** (2026-10-01) — one station through two KiwiSDRs far
      apart, combined before decoding (the user's choice of next feature; Dream has
      no such thing).
      *Done:* `rx::diversity`: a `DiversityReceiver` of two full receivers in branch
      mode (`ReceiverConfig::diversity_branch`: the chain hands each multiplex frame's
      equalised MSC cells out, `ReceiverEvent::MscCells`, instead of decoding them;
      `MscDecoder` extracted from the chain and shared). The combiner pairs the
      frames carrying the same transmitted frame, maximum-ratio combines them and
      decodes once. Weights: |H|²/σ² per cell, σ² per branch and frame from the
      decision-directed error times |H|² (as the WMER), in two passes, the second
      against the first combination's decisions (a weak branch's noise reads low
      against its own wrong decisions; without the second pass combining with a very
      poor branch came out 2 % worse than the good branch alone). Pairing: frames
      numbered per branch from the input time since the branch's previous frame,
      snapped to the frame's super-frame position (jitter and the Kiwis' clock drift
      do not matter; an absolute time grid flipped numbers near half-way points); the
      second branch anchored by content (hard-decision agreement 6σ above chance,
      1/points); every pair checked the same way, three mismatches or a branch falling
      more than `MAX_LAG` (8) frames behind the decoded ones (input lost: a Kiwi
      reconnection) re-anchor it. Frame n is decoded once both gave it or none can any
      more (went past it, or lags more than `MAX_LAG`); `flush()` at the end of input.
      Engine: `InputSpec::Diversity` (two sources read side by side with a short wait
      each; one ending leaves the other running), `Session::new_diversity`/
      `push_branch`/`flush` (a branch losing sync keeps the multiplex while the other
      holds; status and plots from the branch with the better SNR, the MSC
      constellation from the combined cells), snapshot `diversity` and `input.kiwi2`.
      CLI `--kiwi2`; GUI *2nd KiwiSDR* field, *Find…* context menu, status strip
      (both Kiwis, *Diversity*). Tests: combiner units (pairing by content then time,
      SNR weights, a lone branch, a branch that stops, a stream that skips, another
      station's frames, clock phase and drift), `tests/diversity.rs` (two independently
      fading channels, branch B 0.9 s late on its own clock: channel 3 at 13 dB 92
      frames vs 61 / 32 alone of 100; the ignored sweep over channels 1–4 and 8–20 dB:
      never below the better branch, e.g. AWGN 8 dB 124 vs 0 / 0, channel 4 10 dB 97 vs
      6 / 1, channel 3 12 dB 137 vs 26 / 28 of 150), and two stand-in Kiwis end to end
      through the engine. Live: CNR1 6030 kHz via Mishima and Osaka, 133 frames
      combined, 140 of 145 multiplex frames correct, no audio concealed (Mishima alone a
      minute before: 110 of 132, 20 concealed).
- [x] **Plots at 60 Hz** (2026-10-01) — the plots follow the signal as fast as it
      changes, at the user's request (they updated at 10 Hz).
      *Done:* `EngineConfig::publish_interval` (100 ms by default, so the CLI is
      unchanged; the GUI asks for 1/60 s and reads live input in pieces that short);
      `Engine::snapshot_if_newer` (the GUI polls every frame and copies new snapshots
      only; any change to the shared snapshot bumps `seq`). The chain keeps the last
      frame's FAC/MSC cells and super frame's SDC cells as sliding windows, and the
      channel power per symbol (`chan_rows` with a running `chan_seq`); the input
      spectrum keeps a lightly averaged row per FFT (`waterfall_rows`, `spectrum_seq`).
      Snapshots carry the latest few rows and the GUI appends those it has not seen, so
      none is lost although publishes come in bursts (taking the latest row per
      snapshot got only half of the FFTs). The delay–Doppler map is made when visuals
      are taken and new symbols came (`ChannelHistory::map`), not per symbol (it costs
      ~4 ms). Waterfall and fading map show a fixed 600 rows, so they scroll as fast as
      they update (2.3× and 3.75× the old speed), from ring textures: a new row is one
      partial upload, the image is drawn with its texture coordinates shifted (vertical
      repeat), and rows keep the colours they arrived with. Repaints run at 60 Hz:
      egui starts a requested repaint a predicted frame time early, so that time is
      added back; requesting 1/60 s plainly ran at the display's 165 Hz (90–100 % of a
      core). Measured: 7–9 % of a core for the whole process at 60 Hz, against 3 % at
      10 Hz.
- [x] **KiwiSDR client** (2026-10-01) — DecDRM tunes a KiwiSDR itself and decodes its
      I/Q, instead of a browser tab and a virtual audio cable.
      *Done:* `decdrm-kiwi`. The protocol follows the reference client kiwiclient
      (jks-prv): `SET auth`, `SET AR OK` on `audio_rate`, the receiver set up on
      `sample_rate` (squelch and generator off, `ident_user`, `mod=iq` ±5 kHz at the DRM
      frequency, AGC, compression off), `SET keepalive` once a second; `SND` blocks
      (flags, sequence, S-meter, 10-byte GPS stamp, big-endian I/Q; the first block,
      left over from the channel's previous user, dropped); `load_cfg` gives the name and
      location. Live findings (firmware 1.902, six public Kiwis): kiwiclient's
      `/<ts>/SND` is upgraded but then ignored, so the client opens the web client's
      sound-only type `/no_wf/<ts>/SND` (a channel without a waterfall when free), with
      `/<ts>/SND` as the fallback after 5 s of silence; the kiwisdr.com proxy redirects
      with `HTTP/1.0 307` to `<id>.proxy2.kiwisdr.com` (port 80), which tungstenite's
      handshake rejects, so the upgrade handshake is our own (accept key checked) and
      tungstenite only frames; owners can cap the channels of "apps" (clients that
      never loaded the web page): such a Kiwi admits DecDRM and sends `too_busy=N` a few
      seconds later. Policy: nothing that refuses (busy, password, app cap, down, an
      offline proxied Kiwi) or closes the session cleanly (time limits) is retried, nor
      a Kiwi that never streamed; a connection lost after streaming is retried with
      backoff (2 s doubling to 30 s, 10 in a row); HTTP and Kiwi redirections followed;
      no pretending to be a browser. `KiwiStream` runs the connection on its own thread
      with a 30 s FIFO; stopping shuts the socket down from the caller, so a slow Kiwi
      never holds up the engine or the GUI. Engine `InputSpec::Kiwi` (I/Q forced, the
      resampler from the reported rate, Kiwi log and status in the snapshot), CLI
      `decdrm rx --kiwi`, GUI *KiwiSDR* source (address with recent ones, frequency,
      name, password kept for the run), status strip (state, S-meter, name), *Find a
      KiwiSDR* (the public list from rx.linkfanel.net, downloaded on request; filtered
      to Kiwis that allow apps, have a free channel and receive the frequency), and a
      double-click in the *Schedule* tab. Tests: protocol and address units, the client
      against `mock::MockKiwi` (bit-exact I/Q, set-up commands, keepalive rate,
      refusals, reconnection, clean close, HTTP/1.0 and Kiwi redirections, path
      fallback, stop within 1 s), and a station's I/Q decoded end to end through the
      stand-in (205 audio frames, none concealed). Live: KCBS on 6140 kHz locked through
      Kiwis in Japan (25.8 dB SNR, every 16-QAM MSC frame correct, at a Kiwi that then
      applied its app cap) and streamed 60 s without interruption elsewhere.
      *Retuning* (2026-10-01): `KiwiStream::tune` sends `SET mod=iq … freq=` on the
      open connection (as the Kiwi's web page and kiwirecorder's scanning do), and a
      reconnection sets up the new frequency. Unread samples are discarded, and blocks
      are dropped until `RETUNE_SETTLE` (1 s) after the command. On the Mishima Kiwi the
      S-meter followed 0.3–0.5 s after it, so 0.5 s was too tight. Engine
      `Command::Tune` restarts the receiver as a new station (the selection and the
      frame counts go too). In the GUI the frequency box stays live while a Kiwi runs:
      the other source controls are locked one by one, so the row wraps as before. A
      typed value is applied on Enter and a drag on release, and a *Schedule*
      double-click retunes the running Kiwi. Tests: the retune on the open connection
      (no samples before the settling time), a reconnection keeping the new frequency,
      and end to end (the station found and decoded again on the same connection).
      Live: KCBS 6140 → CNR1 6030 kHz on one connection; CNR1 found 1.9 s after the
      retune, xHE-AAC audio from 4.3 s.
- [x] **Web stream input** (2026-10-01) — the transmitter relays an internet radio
      stream: `[service.audio.input] url = "…"`, with `stream_titles` (default on).
      *Done:* `decdrm_station::webstream`. HTTP/1.1 client of our own (SHOUTCAST's
      `ICY 200 OK`, chunked coding, redirects, reads that poll a stop flag and give up
      after a stall; rustls with the ring provider and the system's roots); M3U/M3U8/PLS
      resolve to their first entry (relative entries, dot segments), HLS refused. ICY
      metadata stripped, `StreamTitle` parsed (apostrophes, Latin-1). Format by sniffing
      (two agreeing frame headers), then `Content-Type`: MP3/MP2 (own framer, symphonia
      decoder), AAC/HE-AAC/HE-AAC v2 in ADTS (`decdrm_codecs::FdkAdtsDecoder`; PS known
      by FDK's flag, as FDK delivers mono HE-AAC as stereo), Ogg Vorbis (symphonia), Ogg
      Opus (libopus; pre-skip, output gain), FLAC native (CRC-16 framing; a synthetic
      STREAMINFO when joined mid-way) and in Ogg. Our Ogg demuxer follows Icecast's
      chained streams and takes titles from their comments (symphonia's stops at a
      chain). A worker thread decodes, mixes/resamples to the encoder and fills a FIFO,
      waiting when it is full. Sound-card output: the sound-card input's drift loop,
      shared as `ClockFollower`, with a network tuning (1.5 s backlog, 250 ms → 1000 ppm,
      20 s filter: ±0.2 s of jitter → < 200 ppm ripple, simulated); the first read lets
      the server's connection burst arrive and drops it beyond the start-up backlog
      (real time: 1.51 ± 0.02 s); re-buffering after an underrun, skipping beyond 2 s
      over the target; a response with a length (a file) is neither trimmed nor
      skipped. File output: the stream paces the station. Losses → silence and
      reconnection with backoff 1–30 s; the first connection's failure is the station's
      error. Titles are queued with their FIFO position and go on air with their audio,
      first in the text message cycle (`TextMessageEncoder::set_messages`: segment
      boundary, toggle bit inverted). Status `WebStreamStatus` (state, codec, bit rate,
      rate, channels, station name, genre, title, buffer, reconnects, underruns,
      errors), log via `Station::take_log` (printed by `decdrm tx`). Tests against
      servers on 127.0.0.1 with streams made from tones (FDK ADTS encoder, minimal MP3
      and Vorbis writers coding one MDCT line, libopus + Ogg muxer, flacenc), TLS with a
      test CA, playlists, redirects, dropped connections, errors, pacing, and a station
      loopback through the receiver; live through the virtual cable: 80 s, 0 underruns,
      titles received. Not verified: real Icecast/SHOUTCAST servers and real encoders'
      streams (no internet in development).

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
