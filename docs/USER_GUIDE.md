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

### From a KiwiSDR, directly

DecDRM connects to a KiwiSDR itself: it tunes the Kiwi to the DRM frequency in IQ mode
(±5 kHz around it) and decodes the Kiwi's I/Q. No browser or virtual cable is needed.

```bash
decdrm rx --kiwi kiwisdr.example.org --freq 6140 --play
decdrm rx --kiwi "http://kiwi.example:8073/?f=6140iqz10" --duration 600 --out kcbs.wav
```

- **GUI:** choose *KiwiSDR*, enter the Kiwi's address and the frequency in kHz, and
  press *Start*. The address can be `host`, `host:port` (port 8073 if left out) or a
  URL copied from the browser, whose `f=` also sets the frequency; the arrow next to it
  lists the Kiwis used before. The status strip shows the connection, the Kiwi's
  S-meter and its name (location, firmware and more on hover); the log shows the
  connection's events.
- **Finding a Kiwi:** *Find…* lists the public KiwiSDRs (from kiwisdr.com/public, as
  published by rx.linkfanel.net; downloaded only when you press *Update list*). By
  default it shows the Kiwis whose owners allow apps, that have a free channel and that
  receive the frequency; search by place, name or antenna. Click a Kiwi to use it,
  double-click to use it and start.
- **From the schedule:** in the *Schedule* tab, double-click a broadcast (or right-click
  it) to receive it on the KiwiSDR. Without a Kiwi chosen yet, the list opens first.
- **Courtesy and limits.** A Kiwi has only a few channels (often 4–8), shared by
  everyone:
  - DecDRM appears as "DecDRM" in the Kiwi's list of users (change the name in the ⚙
    menu). It takes a channel without a waterfall when one is free, leaving those to
    the Kiwi's web page.
  - It does not try again when a Kiwi refuses it: all channels busy, a password needed
    (enter it in the ⚙ menu; it is not saved), or the owner's limit for apps other than
    the Kiwi's web page. Many owners allow none or one; such a Kiwi lets DecDRM in and
    drops it after a few seconds, and DecDRM says why. The *Find…* list shows how many
    channels each owner gives apps.
  - It does not reconnect when the Kiwi ends the session (time limits). A connection
    lost on the way is retried, with growing pauses.
- **Bandwidth:** a Kiwi's I/Q runs at about 12 kHz, enough for DRM channels up to
  10 kHz. 18 and 20 kHz channels need a wider receiver.

### From a web SDR through a virtual audio cable

For other web SDRs, or a KiwiSDR whose owner admits only its web page, take the SDR's
audio from a *virtual audio cable*: the browser plays the SDR's audio into the cable, and
DecDRM records the other end.

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
   decdrm rx --device "CABLE Output" --format iq --play --volume 70
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

### Station schedule: what is on the air

To know what to tune the SDR to, DecDRM lists the DRM broadcasts scheduled right now,
like Dream's *Stations* dialog. It reads two kinds of schedule:
- **EiBi** (eibispace.de): every shortwave broadcast, one file per season —
  `sked-a26.csv` from the last Sunday of March, `sked-b26.csv` from the last Sunday of
  October. DecDRM shows the DRM ones, which EiBi marks with DIGITAL after the station
  name (*BBC DIGITAL*, *KCBS DIGITAL*), or every broadcast on request. EiBi's codes are
  spelled out: languages, target areas, countries and transmitter sites.
- **Dream (DRMDX)**: the DRM-only `DRMSchedule.ini` that Dream's *Stations* dialog
  downloads.

Nothing is downloaded until you ask for it:

```bash
decdrm schedule --update                 # download EiBi's current file, then list what is on
decdrm schedule                          # DRM broadcasts on the air now (or within 15 min)
decdrm schedule --all                    # the whole DRM schedule, on-air entries marked *
decdrm schedule --freq 6140              # what is scheduled within ±5 kHz of 6140 kHz
decdrm schedule --at 2026-10-03T18:00Z   # at another time (UTC)
decdrm schedule --source dream --update  # Dream's DRMDX list instead
decdrm schedule --sources                # the sources, their URLs and local files
```

`--all-broadcasts` adds EiBi's analogue broadcasts, `--filter TEXT` keeps entries whose
station, language, target, country or site contains the text, and `--dir DIR` uses
another folder.

In the GUI, the **Schedule** tab next to *History* shows the same:
- the UTC clock, the source, *On air now* or *All*, and a filter;
- *Update schedule*, which downloads in the background;
- a coloured dot per entry: green on the air, orange ending within 10 minutes, yellow
  starting within 15 minutes, grey off.

The frequency you receive is highlighted. It comes from the *Frequency* box, or from a
recording's file name: KiwiSDR names recordings like
`…_2026-09-30T12_46_51Z_6140.00_iq.wav`, HDSDR and SDR# like `…_6140kHz_…`. The line
above the table names the station on the air at the recording's time, and the log names
it when you press *Start*. Clicking a row copies its frequency to the clipboard (to
paste into the SDR) and highlights it.

The files live in the `schedule` folder next to the GUI settings (*Folder* opens it).
Downloads use `curl`, which comes with Windows 10/11, or `wget`; if neither works,
download the file in a browser and save it there under the name shown. A file for an
earlier season stays in use, with a warning, until you update. More sources go into
`sources.toml` in that folder:

```toml
[[source]]
name = "mylist"                                # decdrm schedule --source mylist
title = "My DRM list"
format = "eibi"                                # or "dream"
url = "https://example.org/drm-{season}.csv"   # {season} becomes a26, b26, …
```

A `[[source]]` named `eibi` or `dream` changes that built-in source, for example its URL.

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
| Spectrum, Waterfall | the input spectrum, and its history over the last minutes (the waterfall fits the DRM signal once one is found; *Fit to the DRM signal* below it switches back to the whole band) |
| Constellations | FAC, SDC and MSC cells against the ideal points |
| Audio | spectrum of the decoded audio |
| Channel | the channel's magnitude and group delay per carrier |
| Fading | the channel's gain per carrier over the last minute: frequency-selective fades as dark notches, 1/delay apart for two paths of different delay, moving when their Doppler shifts differ |
| Impulse response | power-delay profile, with the guard interval and delay spread |
| Delay–Doppler | the propagation paths of the last 6 s: each spot is a path at its delay (from the receiver's timing) and Doppler shift (from the frequency the receiver tracks); spread along the Doppler axis is how fast that path fades. Separate ionospheric modes show as separate spots; the lines mark the guard interval |
| SNR per carrier | where in the band the noise or interference sits |
| History | SNR/MER/WMER, Doppler, delay, SRO over the last five minutes of signal, and FAC/SDC/MSC/audio error rates per 10 s |
| Schedule | the DRM broadcasts on the air now, from EiBi's or Dream's schedule (see [Station schedule](#station-schedule-what-is-on-the-air)) |

The side panel shows the broadcast clock (with the station's local time when it sends
one), alternative frequencies, the services, the text message, audio details and the
data services. *Log* at the top shows the receiver's log.

The services appear as four bars, one per Short Id, as in Dream. Each bar shows:
- the label and the bit rate of its audio stream (for a data service, of its data
  streams);
- tags for:
  - the codec (HE-AAC, HE-AAC v2, AAC, xHE-AAC, Opus, EnCodec), SBR, PS / Stereo /
    Mono, and the core/output rate;
  - MPEG Surround with the channel set-up the station signals (5.1, 7.1, "other mode"
    given in the surround data, or a reserved code). DecDRM plays the mono or stereo
    core, never surround;
  - EEP, or UEP with part A's share;
  - text messages;
  - attached data applications with their stream's bit rate;
  - conditional access, or a missing decoder;
- language, programme type and country.

Click a bar to decode that service; hover for the details (service ID, streams, packet
ids).

Labels, text messages and data in other scripts use the system's fonts; DecDRM loads
each one the first time such text appears, and the log names it:
- Korean, Chinese and Japanese;
- Arabic and Hebrew;
- Indic scripts, Thai and Ethiopic.

On Windows the fonts that ship with the system cover all of these. On Linux, install
e.g. Noto Sans CJK and Noto Sans for these scripts; DecDRM asks fontconfig for them.
Right-to-left text (Arabic, Hebrew) shows its letters, but in stored order and without
Arabic letter joining: the GUI toolkit (egui) has no bidirectional text support.

The *Audio* section under the text message shows the decoder and playback figures
and the **volume** slider. The slider acts at once, even on audio already queued, and
leaves recordings and the audio spectrum unchanged. It uses a squared law, so 50 % is
about −12 dB; in the CLI, use `--volume PERCENT` with `--play`.

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

**EVS from Korean Central Broadcasting (6140 kHz).** KCBS signals its service as data
(application 0x000), which is why Dream shows it as a data service, but it sends 3GPP
EVS speech audio (13.2 kbit/s, 14 kHz audio bandwidth). DecDRM recognises it within
a second and shows it as an **EVS 13.2** audio service. Decoding needs the 3GPP
reference decoder, which DecDRM cannot include (3GPP/ETSI copyright, and EVS is a
patent-licensed codec, so this is for private use). Download
`26443-j00.zip` from https://www.3gpp.org/ftp/Specs/archive/26_series/26.443/ (or
`ts_126443v190000p0.zip` from ETSI) in a browser, put it into `reference/evs/` and
build with the `evs` feature:

```bash
cargo build --release -p decdrm-cli -p decdrm-gui --features decdrm-cli/evs,decdrm-gui/evs
```

Without it the service is marked "no decoder" and its data groups are saved like any
unknown data (`--data-dir`).

The station's encoder writes some EVS frame types (pauses, speech onsets, music) in a
form no standard decoder reads correctly. DecDRM conceals those frames, replaces the
occasional remaining burst (output runs 20 ms behind for that), and fills pauses with
comfort noise shaped like the station's own background. Speech sounds clean; onsets
are a little soft, and the rare music passages stay silent.

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
  - `interleaving` (short/long) and the protection levels: `protection_b` for every
    stream, and `protection_a` for streams with `part = "A"` (unequal error
    protection; it must be more robust, i.e. lower, than `protection_b`).
- **`[output]`:**
  - `file` (WAV/FLAC) and/or `device` (sound card);
  - `format`: `real` IF with the DC carrier at `if_hz`, or `iq`. By default a real
    signal is centred at 12 kHz: the DC carrier sits at 12 kHz for 9/10 kHz, at about
    9.7 kHz for 4.5/5 kHz (their carriers all lie above it) and at about 7 kHz for
    18/20 kHz. Set `if_hz` to place it elsewhere, e.g. lower for a transmitter with
    a narrow audio input;
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
      (a sound card), `url` (an internet radio stream, see
      [below](#relaying-an-internet-radio-stream)) or `tone_hz` (a test tone);
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
- shows the multiplex, per-service bit rates, levels and the transmitted spectrum;
- the *Station* view is a form for the file (the *TOML* view shows the text). *Part A*
  on an audio service or data application moves that stream into the more strongly
  protected part (*Protection, part A*), outlined in the multiplex bar. The part A row
  is greyed out while no stream uses it.

### Live transmission through a sound card

With `[output] device = "…"` the station plays in real time, paced by the sound card.
`device_buffer_ms` (default 400) is how much signal is queued ahead.

A programme taken from another sound card (`[service.audio.input] device = "…"`) runs
on that card's clock. A drift loop trims the input's resampling so that the two cards'
clock difference, typically 50–200 ppm, neither overflows nor starves the capture
buffer. The CLI's status lines show this as *clock trim*.

To hear your own station, point `decdrm rx --device …` (or the GUI) at the other end of
the cable.

### Relaying an internet radio stream

`[service.audio.input] url = "…"` takes the programme from an internet radio stream.
Rebroadcasting someone else's programme needs their permission.

```toml
[service.audio]
codec = "he-aac"
text = ["Relayed by DecDRM"]       # optional; the stream's titles go first

[service.audio.input]
url = "https://radio.example/live.mp3"
# stream_titles = true             # the stream's "now playing" as text messages (default)
```

- **Streams:** Icecast and SHOUTCAST over HTTP or HTTPS (checked against the system's
  trusted certificates), following redirects. A playlist (`.m3u`, `.m3u8`, `.pls`)
  stands for its first stream. HLS (segmented `.m3u8`), DASH, MP4 and WMA streams are
  not supported.
- **Codecs:** MP3 (and MP2), AAC, HE-AAC and HE-AAC v2 in ADTS, Ogg Vorbis, Ogg Opus
  (mono or stereo), FLAC (native or in Ogg). The format is recognised from the data,
  from the server's `Content-Type` only when the data does not tell. The audio is mixed
  or duplicated to the encoder's channels and resampled; `gain_db` applies.
- **Titles:** with `stream_titles` (the default) the title of what is playing — the
  ICY `StreamTitle`, or an Ogg stream's comments — goes out as a text message, first in
  the cycle, followed by the `text` messages. A new title replaces the old one at once
  (receivers see a new message), and it changes when its audio goes out, not when it
  arrives.
- **Timing:** the stream runs on the broadcaster's clock.
  - With a sound-card output the station keeps 1.5 s of the stream in hand and follows
    its clock like a sound-card input (*clock trim* in the CLI's status lines). It
    starts once that much has arrived, sends silence and re-buffers when the stream
    stalls, and skips ahead when it falls more than 2 s behind (after a reconnection).
    A URL that serves a file (the server states its length) is neither trimmed nor
    skipped.
  - With a file output the stream paces the station: the signal is written as fast as
    the stream delivers, which after the server's first burst is real time. A web
    stream does not end: give `--duration` (or use the GUI's *Stop after*).
- **Interruptions:** when the connection drops, the stream ends or cannot be decoded,
  the station sends silence and reconnects after 1, 2, 4, 8 and 15 s, then every 30 s
  (back to 1 s after a connection that lasted a minute). Only the first connection must
  succeed: a wrong URL, an HTTP error, an unsupported format or no audio within 20 s
  stop the station with a message that says so.
- **Status:** the CLI prints connections, playlists, redirects, titles and
  reconnections as they happen and adds the stream's state (connecting, buffering,
  playing, reconnecting), buffer and title to its status lines. The station's status
  also carries the stream's coding, bit rate, sampling rate and station name.

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
| KiwiSDR list (*Find…*) | `kiwi/kiwisdr_com.js` next to the GUI settings (`%APPDATA%\decdrm\kiwi` on Windows) |
| Broadcast schedules | `schedule/` next to the GUI settings (`%APPDATA%\decdrm\schedule` on Windows): `sked-a26.csv` (EiBi), `DRMSchedule.ini` (Dream), optional `sources.toml`; `decdrm schedule --dir DIR` uses another folder |
