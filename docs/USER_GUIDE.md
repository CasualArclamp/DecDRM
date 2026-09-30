# DecDRM user guide

DecDRM receives and transmits Digital Radio Mondiale (DRM30, robustness modes A–D, 4.5–20
kHz). It has a command-line tool, `decdrm`, and a desktop program, `decdrm-gui`. This
guide covers building it, receiving from recordings and from web SDRs, reading the
displays, saving data, and running a transmitter. [DESIGN.md](DESIGN.md) describes the
internals.

- [Building](#building)
- [Receiving](#receiving)
- [Reading the displays](#reading-the-displays)
- [Services, data and logs](#services-data-and-logs)
- [Transmitting](#transmitting)
- [EnCodec (experimental)](#encodec-experimental)
- [Troubleshooting](#troubleshooting)
- [Files and folders](#files-and-folders)

## Building

You need Rust 1.88 or newer (1.95 for the GUI), a C/C++ compiler and CMake for the
bundled codecs (FDK-AAC, libopus, libxaac). On Windows that means the MSVC Build Tools.
On Linux you need gcc plus the ALSA development package (`libasound2-dev` on
Debian/Ubuntu, `alsa-lib-devel` on Fedora).

```bash
git clone --recursive https://github.com/CasualArclamp/DecDRM.git
cd DecDRM
cargo build --release
```

The programs are `target/release/decdrm` and `target/release/decdrm-gui` (`.exe` on
Windows). If you cloned without `--recursive`, run `git submodule update --init
--recursive` first.

## Receiving

### From a recording

```bash
decdrm rx recording.flac                       # real IF or audio, any sample rate
decdrm rx iq.wav --format iq                   # I/Q, I on the left channel
decdrm rx rec.flac --out audio.wav --data-dir received --log reception.csv
decdrm-gui recording.flac                      # the same in the GUI (then press Start)
```

- **Input:** WAV or FLAC at any sample rate. Mono files are taken as a real signal;
  stereo files as real (`--channel mix`, the default; or `left`, `right`, `diff`) or
  as I/Q (`--format iq`, or `iq-swapped` when I is on the right channel). The GUI opens
  files with `IQ` in their name as I/Q; the format box next to the file name changes it.
- **Frequency:** the receiver finds the DRM signal anywhere in the spectrum. It also
  detects an inverted spectrum, as with lower-sideband reception, automatically; turn
  that off with `--no-auto-flip`, or force it with `--flip`.
- **Speed:** a file is decoded as fast as possible (about 100× real time).
  `--realtime` paces it to real time, and `--play` plays the audio, which implies real
  time. The GUI's *Real time* box does the same.

### From a web SDR (KiwiSDR and others) through a virtual audio cable

DecDRM has no network client. It takes a web SDR's audio from a *virtual audio cable*:
the browser plays the SDR's audio into the cable, and DecDRM records the other end.

1. **Install a virtual cable.**
   - **Windows:** for example VB-Audio Virtual Cable. Its playback end is called "CABLE
     Input" and its recording end "CABLE Output".
   - **Linux (PipeWire or PulseAudio):**
     `pactl load-module module-null-sink sink_name=drm`. Then pick "Monitor of drm" as
     DecDRM's input in `pavucontrol` (*Recording* tab) while DecDRM runs; DecDRM uses
     ALSA's `default` device.
2. **Send the browser's audio to the cable.**
   - **Windows:** *Settings → System → Sound → Volume mixer*.
   - **Linux:** `pavucontrol`, *Playback* tab.
3. **Tune the SDR to the DRM station.**
   - Use **IQ mode**, centred on the station, with the passband covering the whole
     signal (usually 10 kHz). A KiwiSDR's audio runs at 12 kHz, so only its IQ mode
     carries a full 10 kHz DRM signal.
   - On an SDR with wider audio, USB or AM with a passband covering the signal also
     works. The signal may sit anywhere in the audio band.
   - Leave the SDR's noise reduction and audio filters off.
4. **Start DecDRM on the cable:**

   ```bash
   decdrm devices                                   # list the sound cards
   decdrm rx --device "CABLE Output" --format iq --play
   ```

   In the GUI, choose *Sound card*, pick the cable, set the format (*I/Q* for IQ mode),
   and press *Start*.
5. **Adjust the level** with the SDR's volume so that the *Level* meter sits well below
   full scale (−30 … −10 dBFS). Clipping ruins the OFDM signal, and very low levels
   waste the sound card's resolution.

The receiver corrects the SDR's sample-rate error, up to about ±1250 ppm. It also
survives the gaps and jumps of a network stream: a timing jump is resynchronised in
about a second. Playback is clock-drift compensated, so it neither underruns nor
drifts out of sync over hours.

## Reading the displays

The status LEDs, left to right:

| LED | Green means |
|---|---|
| Input | the input level is usable: neither silent nor clipping (above −10 dBFS RMS the OFDM peaks start to clip) |
| Time | a DRM signal was found in the spectrum and the symbol timing acquired |
| Frame | frame synchronisation from the time-reference pilots |
| FAC | the Fast Access Channel blocks of the last ~1.3 s passed their CRC |
| SDC | the Service Description Channel blocks of the last ~2.5 s passed their CRC |
| MSC | the multiplex frames of the selected service decode |
| Audio | the selected service's audio frames pass their CRC |

Yellow means partly OK, and red means failing.

- **State:** *Acquisition* (searching), *Tracking* (first FAC decoded, loops
  tightening), *Locked*.
- **SNR:** signal-to-noise ratio from the pilots.
  - Roughly 15 dB is needed for 64-QAM MSC and 9 dB for 16-QAM; fading channels need
    several dB more.
- **MER / WMER:** modulation error ratio of the MSC cells, plain and weighted by
  channel quality. WMER close to MER means the channel is flat.
- **Doppler:** fading rate. **Delay:** delay spread of the multipath (the impulse
  response's width).
  - Mode A copes with little of either and mode D with the most.
- **SRO:** the input's sample-rate offset being corrected, in Hz at 48 kHz (1 Hz ≈ 21
  ppm).
- **DC:** frequency of the DRM signal's centre (DC carrier) in the input.

GUI plot tabs:

| Tab | Shows |
|---|---|
| Overview | input spectrum with the DRM band and DC carrier; FAC/SDC/MSC constellations |
| Spectrum, Waterfall | the input spectrum, and its history over the last minutes |
| Constellations | FAC, SDC and MSC cells against the ideal points |
| Audio | spectrum of the decoded audio |
| Channel | the channel's magnitude and group delay per carrier |
| Impulse response | power-delay profile, with the guard interval and delay spread |
| SNR per carrier | where in the band the noise or interference sits |
| History | SNR/MER/WMER, Doppler, delay, SRO over the last five minutes of signal, and FAC/SDC/MSC/audio error rates per 10 s |

The side panel shows the broadcast clock (with the station's local time when it sends
one), alternative frequencies, the services, the text message, audio details and the
data services. *Log* at the top shows the receiver's log.

The services appear as four bars, one per Short Id, as in Dream. Each bar shows:
- the label and the bit rate of its audio stream (for a data service, of its data
  streams);
- tags for:
  - the codec (HE-AAC, HE-AAC v2, AAC, xHE-AAC, Opus, EnCodec), SBR, PS / Stereo /
    Mono, and the core/output rate;
  - EEP, or UEP with part A's share;
  - text messages;
  - attached data applications with their stream's bit rate;
  - conditional access, or a missing decoder;
- language, programme type and country.

Click a bar to decode that service; hover for the details (service ID, streams, packet
ids).

## Services, data and logs

### Services and audio

A multiplex carries up to four services. The first audio service is decoded unless you
choose another: click it in the GUI, or use `--service N` in the CLI (N = the Short Id,
0–3).

Supported audio:

| Codec | Notes |
|---|---|
| AAC, HE-AAC, HE-AAC v2 (parametric stereo), xHE-AAC | FDK-AAC |
| Opus | Dream's extension, all three signalling variants |
| EnCodec | DecDRM's own extension, see below |

Old CELP/HVXC streams are reported as unsupported. Text messages appear under the
services; lost audio frames are concealed.

### Data services

| Application | GUI | Saved with `--data-dir DIR` (GUI: *Data info → Folder…*) |
|---|---|---|
| MOT Slideshow | *Slideshow* tab | `DIR/slides/` |
| Broadcast Website | *Website* tab; *Open in browser* opens the HTML start page | `DIR/website/service<N>/` |
| Journaline | *Journaline* tab (news pages, links) | — |
| EPG (programme guide) | *EPG* tab (the programmes per service, the one on air highlighted) | `DIR/epg/*.xml` (TS 102 818 XML) |
| TPEG | counted in *Data info* | `DIR/raw/service<N>_tpeg.bin` |
| Other applications | counted in *Data info* | `DIR/raw/service<N>_app<XXX>.bin` |

- **Raw captures** hold the data fields of the MSC data groups received with a valid
  CRC, one after the other; for TPEG that is the stream of TPEG transport frames.
- **Synchronous stream mode** services go to `DIR/raw/service<N>_stream_app<XXX>.bin`.
- Each run starts its capture files afresh.

Without a data folder the GUI still writes Broadcast Website files, because a browser
needs files. They go below `websites/<service id>/` next to its settings file (see
[Files and folders](#files-and-folders)).

### Clock and alternative frequencies

The broadcast time comes from the SDC and is shown as UTC and, when signalled, the
station's local time. It updates once a minute.

Alternative frequencies are listed one per line:
- this multiplex on other frequencies;
- the services on DRM, AM, FM or DAB, with their ids;
- their schedules and regions.

A list whose schedule applies at the broadcast time is marked *active*.

### Reception logs

`decdrm rx … --log reception.csv` writes one row per second of signal (`--log-interval`
sets the spacing). The columns are:

`utc, signal_s, state, mode, bandwidth_khz, dc_hz, sro_hz, snr_db, mer_db, wmer_db,
fac_mer_db, doppler_hz, delay_ms, input_dbfs, fac_ok, fac_bad, sdc_ok, sdc_bad,
msc_frames, msc_ok, msc_bad, audio_ok, audio_concealed, service_id, label, codec`

The counters are cumulative. A `.jsonl` file name gives JSON Lines instead: the same
rows as `"type": "metrics"` objects, plus events such as text messages, data objects and
log lines.

## Transmitting

> **Keep DRM signals off the air unless you hold a licence.** Feed the signal into a
> virtual cable, a dummy load, a receiver's IF input or your own SDR loopback.

A *station file* (TOML) describes the whole transmission:
- the channel;
- where the signal goes;
- the clock;
- up to four services with their audio and data;
- alternative frequencies.

[`crates/decdrm-station/examples/station.toml`](../crates/decdrm-station/examples/station.toml)
is a complete, commented example. Copy it and edit it.

```bash
decdrm tx station.toml --check                      # validate, show the multiplex, exit
decdrm tx station.toml --duration 60 --output drm.wav
decdrm tx station.toml                              # the configured outputs, e.g. a sound card
```

`--check` reports every problem at once, with its location. It then prints the
multiplex: streams, bit rates, codec settings and what goes where.

### The station file in brief

- **`[channel]`:**
  - `mode` (A–D) and `occupancy` (0–5 = 4.5, 5, 9, 10, 18, 20 kHz);
  - `msc_mode` (16-QAM, 64-QAM, HMsym, HMmix) and `sdc_mode`;
  - `interleaving` (short/long) and the protection levels.
- **`[output]`:**
  - `file` (WAV/FLAC) and/or `device` (sound card);
  - `format`: `real` IF at `if_hz` (default 12 kHz), or `iq`;
  - `level_dbfs` (RMS level; OFDM peaks are about 10 dB higher) and `band_limit`.
- **`[time]`:** sends the SDC clock from the system time or a fixed `start`, with an
  optional local time offset.
- **`[[service]]`:** `label`, 24-bit `id`, FAC language and programme type, and
  optional ISO language and country. Each service is either:
  - an **audio service** with `[service.audio]`:
    - `codec` = `aac`, `he-aac`, `he-aac-v2`, `xhe-aac`, `opus` or `encodec`;
    - `core_rate`, `stereo`;
    - `text = [...]`, text messages sent in turn;
    - `[service.audio.input]`, exactly one of `file` (any WAV/FLAC, `loop`), `device`
      (a sound card) or `tone_hz` (a test tone);
  - a **data service** with `[service.data]`.
- **Data applications** go in `[service.data]` or `[[service.app]]`, which rides along
  with an audio service:

  | `type` | Content |
  |---|---|
  | `slideshow` | a folder of JPEG/PNG images |
  | `website` | a directory tree with a start page |
  | `journaline` | a TOML/JSON page file |
  | `epg` | inline `[[…programme]]` entries and/or a file; the guide describes the station's audio service |
  | `tpeg` | a file, sent as TPEG |
  | `raw` | a file, sent as application `app_id` |

  Each application requests a `bitrate`, rounded up to whole packets. Applications can
  share one packet stream (`stream = "name"`); `part = "A"` and `hierarchical = true`
  place streams in the better-protected part or the hierarchical layer.
- **The audio streams** get whatever capacity the data leaves, and the encoders' bit
  rates follow from that.
- **`[afs]`:** alternative frequencies:
  - `[[afs.multiplex]]`: this multiplex elsewhere, in `khz`;
  - `[[afs.other]]`: a service on `drm`, `am`, `fm` (`mhz`) or `dab` (`channels`);
  - `[[afs.schedule]]` and `[[afs.region]]`, which the lists refer to.

### Test signals: the channel simulator

`[simulate]` passes the signal through a DRM channel model before the outputs:
- channel models 1–6 of ES 201 980 annex B (multipath with Rayleigh fading);
- white noise at a given SNR;
- a frequency offset;
- a receiver clock error.

Use it to test receivers, DecDRM or others, against a known channel:

```bash
decdrm tx station.toml --duration 60 --output ch3_18dB.wav --channel-model 3 --snr 18
decdrm rx ch3_18dB.wav                  # the SNR shown should be close to 18 dB
```

The SNR is measured in the nominal channel bandwidth (10 kHz for a 10 kHz signal), as
the receiver reports it. Roughly, 64-QAM needs 15 dB on channel 1 and 22–25 dB on the
fading channels 3–5; 16-QAM needs about 6 dB less.

### The GUI's Transmitter tab

The GUI's *Transmitter* page edits the station file:
- *New*, *Open…*, *Save*, *Save as…*;
- *Validate* marks problems at their line;
- starts and stops the transmission, optionally with *Stop after* a duration;
- shows the multiplex, per-service bit rates, levels and the transmitted spectrum.

### Live transmission through a sound card

With `[output] device = "…"` the station plays in real time, paced by the sound card.
`device_buffer_ms` (default 400) is how much signal is queued ahead.

A programme taken from another sound card (`[service.audio.input] device = "…"`) runs
on that card's clock. A drift loop trims the input's resampling so that the two cards'
clock difference, typically 50–200 ppm, neither overflows nor starves the capture
buffer. The CLI's status lines show this as *clock trim*.

To hear your own station, point `decdrm rx --device …` (or the GUI) at the other end of
the cable.

## EnCodec (experimental)

EnCodec is Meta's neural audio codec, which DecDRM carries as its own audio coding (SDC
audio coding 10 with an `ENC1` configuration). Other receivers, including Dream, ignore
such services. It runs 1.5–24 kbit/s with CRC-protected layers and conceals lost frames
well: at 15 dB SNR it lost 2 % of frames where HE-AAC lost 26 %. It runs about 15× real
time on a desktop CPU.

```bash
cargo build --release -p decdrm-cli -p decdrm-gui --features decdrm-cli/encodec,decdrm-gui/encodec
decdrm models download encodec        # ~93 MB, checked by SHA-256
decdrm models list                    # where the weights are looked for
```

In the station file set `codec = "encodec"`, optionally with `bandwidth_kbps`. Weights
are looked for in `DECDRM_MODELS`, then in `models/` next to the program and in its
parent directories.

## Troubleshooting

**Nothing is found (the *Time* LED stays off).**
- Check the level: the *Level* meter, or `input_dbfs` in the log.
- Check the format: real versus I/Q, and the right channel of a stereo input.
- Check that the SDR's passband covers the whole DRM signal.
- The *Spectrum* tab should show the flat, roughly 10 kHz wide DRM block. If the block
  is cut off by the SDR's filter, widen the passband or use IQ mode.

**Found, but no FAC (Time and Frame on, FAC red).**
- The signal is too weak or too distorted. Look at the SNR, the constellations and the
  *SNR per carrier* tab for interference.
- A signal clipped by a high level looks like this too: lower the SDR's volume.

**FAC and SDC fine, but no audio.**
- The MSC needs more SNR than the FAC, about 15 dB for 64-QAM.
- Choose the audio service if a data service is selected.
- Check the log for "unsupported" (CELP/HVXC, or EnCodec in a build without it).

**Audio drops out or stutters in live use.**
- Check the log for resynchronisations, which come from network gaps in the SDR
  stream.
- Check the *Buffer* and *Drift* figures of the audio output. The drift loop needs a
  few seconds after the start.

**The GUI is slow.** While the receiver runs it redraws ten times a second, which costs
a few percent of one core. Moving the mouse over the window adds a frame per event.

**Linux: no sound devices.** DecDRM uses ALSA through cpal. With PipeWire or PulseAudio
it records ALSA's `default` device; route that to the cable's monitor as described in
[step 1 above](#from-a-web-sdr-kiwisdr-and-others-through-a-virtual-audio-cable).

## Files and folders

| What | Where |
|---|---|
| GUI settings | `%APPDATA%\decdrm\gui.toml` (Windows), `$XDG_CONFIG_HOME/decdrm/gui.toml` or `~/.config/decdrm/gui.toml` (Linux); `--config FILE` uses another file |
| GUI's website copies (no data folder set) | `websites/<service id>/` next to the settings file |
| Received objects | the data folder (`--data-dir`, GUI *Data info → Folder…*): `slides/`, `website/`, `epg/`, `raw/` |
| EnCodec weights | `models/encodec_24khz/model.safetensors` (see `decdrm models list`) |
