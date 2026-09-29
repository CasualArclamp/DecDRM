//! xHE-AAC transmitter → receiver round trips: [`XheAacEncoder`] access units go through
//! the real DRM audio super-frame builder and parser of `decdrm-core`
//! (`XheAacFramer` / `XheAacDeframer`, ES 201 980 §5.3.1) and are decoded by FDK-AAC opened
//! from the SDC entity 9 bytes, exactly as the receiver does.
//!
//! The test plays the transmitter's frame loop: every 400 ms it builds one audio super
//! frame of the stream's fixed length, having fed the encoder the audio up to the end of
//! that super frame plus two frames (the encoder must run ahead of the channel, see the
//! `xhe_enc` module docs).

mod common;

use common::{channel, db, peak_frequency, tone_amplitude};
use decdrm_codecs::{
    AudioInfo, DrmAudioCoding, XheAacConfig, XheAacEncoder, XheSbrMode, XheSbrRatio, open_decoder,
};
use decdrm_core::mux::audio::{XheAacDeframer, XheAacFramer, xhe_frame_crc_ok};
use decdrm_core::mux::service::{AudioCodec, AudioMode, AudioParams};
use std::f64::consts::PI;

/// Tones of the synthetic programme: (frequency, amplitude) per channel.
const LEFT: [(f64, f64); 2] = [(440.0, 0.35), (1250.0, 0.15)];
const RIGHT: [(f64, f64); 2] = [(660.0, 0.35), (1250.0, 0.15)];

fn tones(ch: usize, channels: usize) -> &'static [(f64, f64)] {
    if channels == 1 || ch == 0 {
        &LEFT
    } else {
        &RIGHT
    }
}

/// `n` instants of the interleaved tone mix starting at instant `start`.
fn programme(fs: u32, channels: usize, start: usize, n: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n * channels);
    for i in start..start + n {
        let t = i as f64 / f64::from(fs);
        for ch in 0..channels {
            let v: f64 = tones(ch, channels)
                .iter()
                .map(|&(f, a)| a * (2.0 * PI * f * t).sin())
                .sum();
            out.push(v as f32);
        }
    }
    out
}

/// Least-squares fit of DC plus sine/cosine pairs at `freqs` to `x`; returns
/// (fitted tone power, residual power, residual signal).
fn tone_fit(x: &[f64], fs: u32, freqs: &[f64]) -> (f64, f64, Vec<f64>) {
    let n = x.len();
    let k = 1 + 2 * freqs.len();
    let basis = |j: usize, i: usize| -> f64 {
        if j == 0 {
            return 1.0;
        }
        let f = freqs[(j - 1) / 2];
        let ph = 2.0 * PI * f * i as f64 / f64::from(fs);
        if j % 2 == 1 { ph.sin() } else { ph.cos() }
    };
    // Normal equations (k ≤ 7, well conditioned for tones far apart over many periods),
    // as an augmented matrix [AᵀA | Aᵀx] solved by Gauss-Jordan elimination.
    let mut a = vec![vec![0.0; k + 1]; k];
    for (i, &xi) in x.iter().enumerate() {
        let b: Vec<f64> = (0..k).map(|j| basis(j, i)).collect();
        for (row, &br) in a.iter_mut().zip(&b) {
            for (cell, &bc) in row.iter_mut().zip(&b) {
                *cell += br * bc;
            }
            row[k] += br * xi;
        }
    }
    for p in 0..k {
        let piv = (p..k)
            .max_by(|&i, &j| a[i][p].abs().total_cmp(&a[j][p].abs()))
            .unwrap();
        a.swap(p, piv);
        let pivot_row = a[p].clone();
        for (r, row) in a.iter_mut().enumerate() {
            if r != p {
                let f = row[p] / pivot_row[p];
                for (cell, &pc) in row.iter_mut().zip(&pivot_row).skip(p) {
                    *cell -= f * pc;
                }
            }
        }
    }
    let coef: Vec<f64> = (0..k).map(|j| a[j][k] / a[j][j]).collect();
    let (mut tone, mut resid) = (0.0, 0.0);
    let mut residual = Vec::with_capacity(n);
    for (i, &xi) in x.iter().enumerate() {
        let fit_tones: f64 = (1..k).map(|j| coef[j] * basis(j, i)).sum();
        tone += fit_tones * fit_tones;
        let e = xi - coef[0] - fit_tones;
        resid += e * e;
        residual.push(e);
    }
    (tone / n as f64, resid / n as f64, residual)
}

/// Power of `x` (first 2^m samples) in the bands between `edges` (Hz), via a radix-2 FFT.
fn band_powers(x: &[f64], fs: u32, edges: &[f64]) -> Vec<f64> {
    let n = 1usize << (usize::BITS - 1 - x.len().leading_zeros());
    let (mut re, mut im): (Vec<f64>, Vec<f64>) = (x[..n].to_vec(), vec![0.0; n]);
    // Bit-reversal permutation, then iterative butterflies.
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (wr, wi) = ((ang * k as f64).cos(), (ang * k as f64).sin());
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
    let df = f64::from(fs) / n as f64;
    edges
        .windows(2)
        .map(|w| {
            let (lo, hi) = (
                (w[0] / df).ceil() as usize,
                ((w[1] / df).floor() as usize).min(n / 2),
            );
            // Parseval: sum over both half spectra, divided by n².
            (lo..hi)
                .map(|k| 2.0 * (re[k] * re[k] + im[k] * im[k]))
                .sum::<f64>()
                / (n * n) as f64
        })
        .collect()
}

struct Outcome {
    description: String,
    ratio: XheSbrRatio,
    rate: u32,
    channels: usize,
    super_frame_bytes: usize,
    out: Vec<f32>,
    out_rate: u32,
    frames_decoded: usize,
    concealed: usize,
    crc_failures: usize,
    underruns: usize,
    /// Encoder bytes attributable to each 400 ms of audio (AUs + CRCs + directory + header).
    per_super_frame: Vec<usize>,
    /// max over time of (bytes produced − bytes the channel carried for the same audio).
    max_excess: i64,
    min_excess: i64,
    max_reservoir_bytes: i64,
    max_pending_after_cut: usize,
    stats: decdrm_codecs::XheEncoderStats,
    net_bitrate: f64,
    core_bitrate: u32,
    static_config: Vec<u8>,
    max_borders: u8,
    /// Frames that started inside the transmitted payload (all but the last one or two of
    /// them are complete).
    frames_started_in_payload: usize,
    /// Super frames with at least one frame border, and how many of them contain the
    /// start of an independent frame (§5.3.1.2 recommends one in each).
    super_frames_with_borders: usize,
    super_frames_with_independent: usize,
}

/// Signal generator: (sampling rate, channels, first instant, instants) → interleaved PCM.
type Programme = fn(u32, usize, usize, usize) -> Vec<f32>;

fn run(cfg: XheAacConfig, seconds: f64) -> Outcome {
    run_with(cfg, seconds, programme)
}

fn run_with(cfg: XheAacConfig, seconds: f64, signal: Programme) -> Outcome {
    let fs = cfg.sample_rate;
    let channels = usize::from(cfg.channels);
    let l = cfg.super_frame_bytes;
    let mut enc = XheAacEncoder::new(cfg.clone()).unwrap_or_else(|e| panic!("{cfg:?}: {e}"));
    let frame = enc.frame_len();

    // SDC entity 9, as the transmitter would signal it (decdrm-core's AudioParams) and as
    // the codecs crate describes it: both must give the same type-9 bytes.
    let info = enc.audio_info();
    let t9 = info.to_type9_bytes();
    let params = AudioParams::new(
        1,
        AudioCodec::XheAac,
        false,
        if channels == 2 {
            AudioMode::Stereo
        } else {
            AudioMode::Mono
        },
        fs,
        false,
        enc.static_config().to_vec(),
    );
    assert_eq!(
        params.type9_bytes, t9,
        "decdrm-core and decdrm-codecs disagree on type 9"
    );
    let parsed = AudioInfo::from_type9_bytes(&t9).unwrap();
    assert_eq!(parsed.drm_audio_coding(), Some(DrmAudioCoding::XheAac));
    assert_eq!(parsed.sample_rate(), Some(fs));
    let mut dec = open_decoder(DrmAudioCoding::XheAac, &t9).expect("FDK accepts the config");

    let mut framer = XheAacFramer::new();
    let mut deframer = XheAacDeframer::new();
    let n_sf = (seconds / 0.4).round() as usize;
    let sf_samples = |k: usize| (k as u64 * 2 * u64::from(fs) / 5) as usize; // 0.4·k·fs
    let chunk = 997; // deliberately unrelated to the frame length
    let mut fed = 0usize;
    let mut frames_out = 0usize;
    // Stream offset (payload byte stream: AU + CRC per frame) and reservoir level of every
    // frame start.
    let mut starts: Vec<(usize, u8, bool)> = Vec::new();
    // Payload byte range of every super frame and its border count.
    let mut payloads: Vec<(usize, usize, u8)> = Vec::new();
    let mut pushed = 0usize;
    let mut sent = 0usize;
    let mut o = Outcome {
        description: String::new(),
        ratio: enc.sbr_ratio(),
        rate: fs,
        channels,
        super_frame_bytes: l,
        out: Vec::new(),
        out_rate: 0,
        frames_decoded: 0,
        concealed: 0,
        crc_failures: 0,
        underruns: 0,
        per_super_frame: vec![0; n_sf + 8],
        max_excess: i64::MIN,
        min_excess: i64::MAX,
        max_reservoir_bytes: i64::from(enc.max_reservoir_bits()) / 8,
        max_pending_after_cut: 0,
        stats: enc.stats(),
        net_bitrate: enc.net_bitrate(),
        core_bitrate: enc.core_bitrate(),
        static_config: enc.static_config().to_vec(),
        max_borders: 0,
        frames_started_in_payload: 0,
        super_frames_with_borders: 0,
        super_frames_with_independent: 0,
    };
    let avg_frame_bytes = enc.net_bitrate() / 8.0 * frame as f64 / f64::from(fs) + 4.0;
    for k in 0..n_sf {
        // Run the encoder ahead of the channel by two frames (and further, one frame at
        // a time, should the queue still not fill the payload: an underrun, which the
        // checks count as a failure).
        let mut target = sf_samples(k + 1) + 2 * frame;
        loop {
            while fed < target {
                let n = chunk.min(target - fed);
                let pcm = signal(fs, channels, fed, n);
                fed += n;
                for au in enc.encode(&pcm).unwrap() {
                    // Audio of this frame starts at frames_out·frame samples.
                    let sf_of_frame = frames_out * frame * 5 / (2 * fs as usize);
                    if let Some(b) = o.per_super_frame.get_mut(sf_of_frame) {
                        *b += au.data.len() + 4;
                    }
                    starts.push((pushed, au.bit_reservoir_level, au.independent));
                    framer.push_access_unit(&au.data, au.bit_reservoir_level);
                    pushed += au.data.len() + 2;
                    frames_out += 1;
                }
            }
            if framer.ready(l) {
                break;
            }
            o.underruns += 1;
            target += frame;
        }
        let pending = framer.pending_bytes();
        let sf = framer
            .next_super_frame(l)
            .expect("the queue fills the payload");
        assert_eq!(sf.len(), l);
        let count = sf[0] >> 4;
        o.max_borders = o.max_borders.max(count);
        let payload = l - 2 - 2 * usize::from(count);
        // Header level: the first frame starting in this super frame's payload, else the
        // frame in progress (§5.3.1.1). The payload starts at stream offset `sent`.
        let level = starts
            .iter()
            .find(|&&(s, _, _)| (sent..sent + payload).contains(&s))
            .or_else(|| starts.iter().rev().find(|&&(s, _, _)| s < sent))
            .map_or(0, |&(_, lv, _)| lv);
        payloads.push((sent, sent + payload, count));
        sent += payload;
        o.max_pending_after_cut = o.max_pending_after_cut.max(pending.saturating_sub(payload));

        // Receiver.
        let (header, frames) = deframer.push(&sf).expect("super frame parses");
        assert!(header.header_crc_ok);
        assert_eq!(header.bit_reservoir_level, level);
        for f in frames {
            if f.crc_ok != Some(true) || !xhe_frame_crc_ok(&f.data) {
                o.crc_failures += 1;
            }
            // The receiver hands FDK the whole audio frame (AU + CRC), as Dream does.
            let pcm = dec.decode(&f.data, None).expect("decode");
            if pcm.concealed {
                o.concealed += 1;
            }
            assert_eq!(usize::from(pcm.channels), channels);
            o.out_rate = pcm.sample_rate;
            o.out.extend_from_slice(&pcm.samples);
            o.frames_decoded += 1;
        }
    }
    // Bytes produced for the audio of the first k super frames versus the channel.
    let mut cum = 0i64;
    for k in 0..n_sf {
        cum += o.per_super_frame[k] as i64 + 2 - l as i64;
        o.max_excess = o.max_excess.max(cum);
        o.min_excess = o.min_excess.min(cum);
    }
    o.stats = enc.stats();
    o.frames_started_in_payload = starts.iter().filter(|&&(s, _, _)| s < sent).count();
    for &(from, to, count) in &payloads {
        if count > 0 {
            o.super_frames_with_borders += 1;
            if starts.iter().any(|&(s, _, ind)| ind && s >= from && s < to) {
                o.super_frames_with_independent += 1;
            }
        }
    }
    o.description = format!(
        "{} {} Hz {:?}, {} bit/s ({} B/SF): libxaac {} bit/s, net {:.0} bit/s, core \
         bandwidth {} Hz, {:.2} frames/SF (avg {:.1} B incl. CRC+dir)",
        if channels == 2 { "stereo" } else { "mono" },
        fs,
        o.ratio,
        cfg.bitrate(),
        l,
        o.core_bitrate,
        o.net_bitrate,
        enc.core_bandwidth(),
        enc.frames_per_super_frame(),
        avg_frame_bytes
    );
    o
}

/// Per channel: tone SNR (dB), [(tone frequency, level error dB)], crosstalk (dB).
type ChannelQuality = (f64, Vec<(f64, f64)>, f64);

struct Quality {
    /// Per channel: (tone SNR dB, [(tone, level error dB)], worst crosstalk dB).
    channels: Vec<ChannelQuality>,
    /// Per channel: residual power (dB relative to the tone power) per band of `BANDS`.
    residual_bands: Vec<Vec<f64>>,
}

const BANDS: [f64; 7] = [0.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 12_000.0, 24_000.0];

fn quality(o: &Outcome) -> Quality {
    let mut q = Quality {
        channels: Vec::new(),
        residual_bands: Vec::new(),
    };
    for ch in 0..o.channels {
        let x = channel(&o.out, o.channels, ch);
        // Steady state: skip 1 s (codec delay, SBR start-up), use up to 2 s.
        let skip = o.out_rate as usize;
        let seg = &x[skip..x.len().min(skip + 2 * o.out_rate as usize)];
        let freqs: Vec<f64> = tones(ch, o.channels).iter().map(|t| t.0).collect();
        let (tone_power, resid, residual) = tone_fit(seg, o.out_rate, &freqs);
        let snr = 10.0 * (tone_power / resid.max(1e-20)).log10();
        q.residual_bands.push(
            band_powers(&residual, o.out_rate, &BANDS)
                .iter()
                .map(|&p| 10.0 * (p / tone_power).max(1e-20).log10())
                .collect(),
        );
        let levels = tones(ch, o.channels)
            .iter()
            .map(|&(f, a)| (f, db(tone_amplitude(seg, o.out_rate, f) / a)))
            .collect();
        // Crosstalk: the other channel's private tone.
        let crosstalk = if o.channels == 2 {
            let other = tones(1 - ch, 2)[0];
            db(tone_amplitude(seg, o.out_rate, other.0) / other.1)
        } else {
            f64::NEG_INFINITY
        };
        q.channels.push((snr, levels, crosstalk));
    }
    q
}

fn report(o: &Outcome, q: &Quality) {
    let s = &o.stats;
    let per_sf = &o.per_super_frame[..o.per_super_frame.len() - 8];
    let (min, max) = per_sf
        .iter()
        .skip(1)
        .fold((usize::MAX, 0), |(a, b), &v| (a.min(v + 2), b.max(v + 2)));
    let mean = per_sf.iter().map(|&v| v + 2).sum::<usize>() as f64 / per_sf.len() as f64;
    eprintln!("{}", o.description);
    eprintln!(
        "  static config {:02X?}; {} frames ({} independent), AU {}..{} B, fill {} B, \
         overruns {}, warnings {}",
        o.static_config,
        s.frames,
        s.independent_frames,
        s.min_frame_bytes,
        s.max_frame_bytes,
        s.fill_bytes,
        s.overruns,
        s.warnings
    );
    eprintln!(
        "  bytes per 400 ms of audio: min {min} / mean {mean:.1} / max {max} vs budget {}; \
         cumulative excess {}..{} B (reservoir {} B); max borders/SF {}; framer backlog \
         after cut ≤ {} B",
        o.super_frame_bytes,
        o.min_excess,
        o.max_excess,
        o.max_reservoir_bytes,
        o.max_borders,
        o.max_pending_after_cut
    );
    eprintln!(
        "  decoded {} frames at {} Hz: {} concealed, {} CRC failures, {} framer underruns; \
         independent frame start in {}/{} super frames with borders",
        o.frames_decoded,
        o.out_rate,
        o.concealed,
        o.crc_failures,
        o.underruns,
        o.super_frames_with_independent,
        o.super_frames_with_borders
    );
    for (ch, (snr, levels, xt)) in q.channels.iter().enumerate() {
        let lv: Vec<String> = levels
            .iter()
            .map(|(f, e)| format!("{f:.0} Hz {e:+.2} dB"))
            .collect();
        eprintln!(
            "  ch{ch}: tone SNR {snr:.1} dB; levels {}; crosstalk {xt:.1} dB",
            lv.join(", ")
        );
        let bands: Vec<String> = BANDS
            .windows(2)
            .zip(&q.residual_bands[ch])
            .map(|(w, p)| format!("{:.0}-{:.0}k {p:.1}", w[0] / 1000.0, w[1] / 1000.0))
            .collect();
        eprintln!(
            "       residual by band (dB re tones): {}",
            bands.join(", ")
        );
    }
}

/// Checks shared by every configuration.
fn check(o: &Outcome, q: &Quality, min_snr_db: f64, level_tol_db: f64) {
    assert_eq!(o.out_rate, o.rate, "decoder output rate");
    assert_eq!(o.crc_failures, 0, "frame CRCs");
    assert_eq!(o.concealed, 0, "no concealment");
    assert_eq!(
        o.underruns, 0,
        "the super-frame builder never ran out of audio frames"
    );
    assert_eq!(o.stats.overruns, 0, "bit reservoir never exceeded");
    assert!(o.max_borders as usize <= 15);
    // The encoder may run ahead of the channel only by its bit reservoir (plus a frame of
    // rounding), and never fall behind it.
    let frame_bytes = o.stats.max_frame_bytes as i64 + 4;
    assert!(
        o.max_excess <= o.max_reservoir_bytes + frame_bytes,
        "encoder exceeded the channel by {} B (reservoir {} B)",
        o.max_excess,
        o.max_reservoir_bytes
    );
    assert!(
        o.min_excess >= -frame_bytes,
        "encoder fell behind the channel by {} B",
        -o.min_excess
    );
    // At most one super frame of backlog beyond the reservoir in the framer.
    assert!(o.max_pending_after_cut as i64 <= o.max_reservoir_bytes + o.super_frame_bytes as i64);
    // Every frame that was completely transmitted was decoded (the frame in progress at
    // the end, and one more if its border was delayed to the next super frame, are not).
    assert!(
        o.frames_decoded + 2 >= o.frames_started_in_payload,
        "{} frames decoded, {} started in the payload",
        o.frames_decoded,
        o.frames_started_in_payload
    );
    // §5.3.1.2: an independent frame should start in every super frame with a border.
    assert!(
        o.super_frames_with_independent * 10 >= o.super_frames_with_borders * 9,
        "independent frame start in only {}/{} super frames",
        o.super_frames_with_independent,
        o.super_frames_with_borders
    );
    for (ch, (snr, levels, xt)) in q.channels.iter().enumerate() {
        let seg = channel(&o.out, o.channels, ch);
        let seg = &seg[o.out_rate as usize..];
        let main = tones(ch, o.channels)[0].0;
        let pf = peak_frequency(seg, o.out_rate, 100.0, 3000.0);
        assert!(
            (pf - main).abs() < 3.0,
            "ch{ch}: strongest tone {pf} Hz, expected {main} Hz"
        );
        for &(f, err) in levels {
            assert!(
                err.abs() < level_tol_db,
                "ch{ch}: {f} Hz level off by {err:+.2} dB"
            );
        }
        assert!(
            *snr > min_snr_db,
            "ch{ch}: tone SNR {snr:.1} dB < {min_snr_db} dB"
        );
        if o.channels == 2 {
            assert!(*xt < -20.0, "ch{ch}: crosstalk {xt:.1} dB");
        }
    }
}

fn stereo(rate: u32, bitrate: u32) -> XheAacConfig {
    XheAacConfig::new(rate, 2, bitrate)
}

#[test]
fn stereo_12kbps_24khz() {
    let o = run(stereo(24_000, 12_000), 4.0);
    let q = quality(&o);
    report(&o, &q);
    assert_eq!(o.ratio, XheSbrRatio::Ratio2To1);
    check(&o, &q, 35.0, 0.5);
}

/// Longer run: the encoder spends its initially full bit reservoir in the first seconds
/// and must then settle at the channel rate.
#[test]
fn stereo_16kbps_24khz_steady_state() {
    let o = run(stereo(24_000, 16_000), 12.0);
    let q = quality(&o);
    report(&o, &q);
    check(&o, &q, 35.0, 0.5);
    // Over the last 8 s the encoder produced exactly the channel capacity (± reservoir).
    let tail: i64 = o.per_super_frame[10..30]
        .iter()
        .map(|&b| b as i64 + 2)
        .sum();
    let budget = 20 * o.super_frame_bytes as i64;
    assert!(
        (tail - budget).abs() <= o.max_reservoir_bytes,
        "last 8 s: {tail} B for a budget of {budget} B"
    );
}

#[test]
fn stereo_24kbps_32khz() {
    let o = run(stereo(32_000, 24_000), 4.0);
    let q = quality(&o);
    report(&o, &q);
    check(&o, &q, 35.0, 0.5);
}

#[test]
fn stereo_32kbps_48khz() {
    let o = run(stereo(48_000, 32_000), 4.0);
    let q = quality(&o);
    report(&o, &q);
    check(&o, &q, 35.0, 0.5);
}

/// The example of ES 201 980 §5.3.1.3: 8 kbit/s mono in robustness modes A-D leaves an
/// average net audio bit rate of 7 660 bit/s (here with 4:1 SBR at 38.4 kHz: 4096-sample
/// frames, 3.75 per super frame).
#[test]
fn mono_8kbps_38k4_spec_example() {
    let o = run(XheAacConfig::new(38_400, 1, 8_000), 4.0);
    let q = quality(&o);
    report(&o, &q);
    assert_eq!(o.ratio, XheSbrRatio::Ratio4To1);
    assert!(
        (o.net_bitrate - 7_660.0).abs() < 1e-6,
        "net {}",
        o.net_bitrate
    );
    check(&o, &q, 30.0, 0.5);
}

/// A rate-control stress programme in 1 s sections: full-scale white noise (the densest
/// spectrum), digital silence, the tone mix with a click every 100 ms (transients, short
/// windows), then the tones alone.
fn stress_programme(fs: u32, channels: usize, start: usize, n: usize) -> Vec<f32> {
    let tones = programme(fs, channels, start, n);
    let mut out = Vec::with_capacity(n * channels);
    for (k, chunk) in tones.chunks(channels).enumerate() {
        let i = start + k;
        let section = i / fs as usize;
        for (ch, &tone) in chunk.iter().enumerate() {
            // Deterministic white noise from a hash of (instant, channel).
            let mut h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (ch as u64 + 1);
            h ^= h >> 29;
            h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            h ^= h >> 32;
            let noise = (h as u32 as f64 / f64::from(u32::MAX) * 2.0 - 1.0) as f32;
            let click = if i % (fs as usize / 10) < 8 { 0.8 } else { 0.0 };
            out.push(match section % 4 {
                0 => 0.7 * noise,
                1 => 0.0,
                2 => (tone + click).clamp(-1.0, 1.0),
                _ => tone,
            });
        }
    }
    out
}

/// The rate control under stress: noise needs more bits than the channel has, silence
/// almost none, clicks force short windows. The stream must stay decodable and within the
/// channel and reservoir limits throughout; the final tone section is checked as usual.
#[test]
fn stereo_16kbps_rate_control_stress() {
    let o = run_with(stereo(24_000, 16_000), 8.0, stress_programme);
    report(&o, &quality(&o));
    assert_eq!(o.crc_failures, 0);
    assert_eq!(o.concealed, 0);
    assert_eq!(o.underruns, 0);
    assert_eq!(o.stats.overruns, 0);
    let frame_bytes = o.stats.max_frame_bytes as i64 + 4;
    assert!(o.max_excess <= o.max_reservoir_bytes + frame_bytes);
    assert!(o.min_excess >= -frame_bytes);
    // No access unit above the xHE-AAC maximum of 6144 bits per channel (§5.3.1.3).
    assert!(o.stats.max_frame_bytes <= 2 * 6144 / 8);
    // The last tone section (7-8 s) decodes cleanly.
    let x = channel(&o.out, 2, 0);
    let seg = &x[7 * 24_000 + 2_400..8 * 24_000 - 2_400];
    let a = tone_amplitude(seg, 24_000, 440.0);
    assert!(
        db(a / 0.35).abs() < 1.0,
        "440 Hz after the stress sections: {a}"
    );
}

/// Every DRM sampling rate × SBR ratio × 8–64 kbit/s, mono and stereo, with fixed SBR
/// ratios (no quality assertions); run with `--ignored --nocapture` to print the numbers.
/// `XHE_TNS=0/1` and `XHE_NF=0/1` override TNS and noise filling.
#[test]
#[ignore]
fn sweep() {
    let mut cases = Vec::new();
    use XheSbrRatio::{None as NoSbr, Ratio2To1 as R21, Ratio4To1 as R41, Ratio8To3 as R83};
    for &(rate, ratios) in &[
        (9_600, &[NoSbr][..]),
        (12_000, &[NoSbr, R21][..]),
        (16_000, &[NoSbr, R83, R21][..]),
        (19_200, &[NoSbr, R83, R21][..]),
        (24_000, &[NoSbr, R83, R21][..]),
        (32_000, &[NoSbr, R83, R21, R41][..]),
        (38_400, &[R83, R21, R41][..]),
        (48_000, &[R83, R21, R41][..]),
    ] {
        for &ratio in ratios {
            for channels in [1u16, 2] {
                if ratio == XheSbrRatio::Ratio4To1 && channels == 2 {
                    continue;
                }
                for bitrate in [8_000u32, 12_000, 16_000, 24_000, 32_000, 48_000, 64_000] {
                    cases.push((rate, ratio, channels, bitrate));
                }
            }
        }
    }
    for (rate, ratio, channels, bitrate) in cases {
        let mut cfg = XheAacConfig::new(rate, channels, bitrate);
        cfg.sbr = XheSbrMode::Fixed(ratio);
        if let Ok(v) = std::env::var("XHE_TNS") {
            cfg.tns = v == "1";
        }
        if let Ok(v) = std::env::var("XHE_NF") {
            cfg.noise_filling = v == "1";
        }
        if let Err(e) = XheAacEncoder::new(cfg.clone()) {
            eprintln!("{rate} Hz {ratio:?} {channels} ch {bitrate}: rejected: {e}");
            continue;
        }
        let o = std::panic::catch_unwind(|| run(cfg.clone(), 3.2));
        match o {
            Ok(o) => {
                let q = quality(&o);
                report(&o, &q);
            }
            Err(_) => eprintln!("{rate} Hz {ratio:?} {channels} ch {bitrate}: PANIC"),
        }
    }
}

/// One configuration from the environment (diagnostics): `XHE_RATE`, `XHE_CH`, `XHE_BR`,
/// `XHE_SBR` (none, 8:3, 2:1, 4:1, auto), `XHE_NF` (noise filling 0/1), `XHE_TNS`,
/// `XHE_INDEP` (independent frame interval), `XHE_SECS`, `XHE_SWITCHED` (0/1).
#[test]
#[ignore]
fn one_from_env() {
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut cfg = XheAacConfig::new(
        var("XHE_RATE", "24000").parse().unwrap(),
        var("XHE_CH", "2").parse().unwrap(),
        var("XHE_BR", "16000").parse().unwrap(),
    );
    cfg.sbr = match var("XHE_SBR", "auto").as_str() {
        "none" => XheSbrMode::Fixed(XheSbrRatio::None),
        "8:3" => XheSbrMode::Fixed(XheSbrRatio::Ratio8To3),
        "2:1" => XheSbrMode::Fixed(XheSbrRatio::Ratio2To1),
        "4:1" => XheSbrMode::Fixed(XheSbrRatio::Ratio4To1),
        _ => XheSbrMode::Auto,
    };
    cfg.noise_filling = var("XHE_NF", "1") == "1";
    cfg.tns = var("XHE_TNS", "1") == "1";
    if let Ok(v) = std::env::var("XHE_INDEP") {
        cfg.independent_interval = Some(v.parse().unwrap());
    }
    if var("XHE_SWITCHED", "0") == "1" {
        cfg.coding_mode = decdrm_codecs::XheCodingMode::Switched;
    }
    let o = run(cfg, var("XHE_SECS", "4").parse().unwrap());
    let q = quality(&o);
    report(&o, &q);
}
