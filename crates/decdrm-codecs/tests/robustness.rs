//! Damaged input must never panic or abort: decoders return concealment (flagged) or an
//! error, and recover when good frames arrive again.

use decdrm_codecs::{
    AacProfile, AudioInfo, AudioMode, CodecError, DrmAudioCoding, DrmAudioDecoder, FdkDrmDecoder,
    FdkDrmEncoder, FdkEncoderConfig, OpusDrmDecoder, OpusDrmEncoder, OpusEncoderConfig, PcmFrame,
    fdk_lib_info, open_decoder,
};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn sane(pcm: &PcmFrame) {
    assert!(
        pcm.channels == 1 || pcm.channels == 2,
        "{} channels",
        pcm.channels
    );
    assert!(
        pcm.sample_rate >= 8_000 && pcm.sample_rate <= 48_000,
        "{} Hz",
        pcm.sample_rate
    );
    assert_eq!(pcm.samples.len() % usize::from(pcm.channels), 0);
    assert!(pcm.samples.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
}

/// Feeds random frames with random CRCs; every call must return Ok (sane audio) or Err.
fn hammer(dec: &mut dyn DrmAudioDecoder, rng: &mut Rng, calls: usize) -> (usize, usize) {
    let (mut ok, mut err) = (0, 0);
    for _ in 0..calls {
        let len = rng.below(400) as usize;
        let frame = rng.bytes(len);
        let crc = rng.next() as u8;
        let r = if rng.below(10) == 0 {
            dec.conceal()
        } else {
            dec.decode(&frame, Some(crc))
        };
        match r {
            Ok(pcm) => {
                sane(&pcm);
                ok += 1;
            }
            Err(_) => err += 1,
        }
    }
    (ok, err)
}

#[test]
fn aac_decoders_survive_garbage() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let configs = [
        AudioInfo::aac(12_000, false, AudioMode::Mono).unwrap(),
        AudioInfo::aac(24_000, false, AudioMode::Stereo).unwrap(),
        AudioInfo::aac(12_000, true, AudioMode::Mono).unwrap(),
        AudioInfo::aac(12_000, true, AudioMode::ParametricStereo).unwrap(),
        AudioInfo::aac(24_000, true, AudioMode::Stereo).unwrap(),
    ];
    for info in configs {
        let mut dec = FdkDrmDecoder::new(&info).unwrap();
        // Concealment before any frame: silence of the nominal geometry.
        let first = dec.conceal().unwrap();
        assert!(first.concealed);
        sane(&first);
        let (ok, err) = hammer(&mut dec, &mut rng, 400);
        eprintln!("{}: {ok} concealed/decoded, {err} errors", dec.describe());
        assert!(ok > 300, "garbage should mostly be concealed, not rejected");
    }
}

#[test]
fn xhe_aac_decoder_survives_garbage() {
    // Minimal DRM xHE-AAC static config: coreSbrFrameLengthIndex 1 (1024, no SBR),
    // noise filling off.
    for stereo in [false, true] {
        let info = AudioInfo::xhe_aac(24_000, stereo, vec![0x00]).unwrap();
        let t9 = info.to_type9_bytes();
        let mut dec = open_decoder(DrmAudioCoding::XheAac, &t9).expect("xHE-AAC config accepted");
        let d = dec.describe();
        assert!(d.starts_with("xHE-AAC (USAC)"), "{d}");
        let mut rng = Rng(42 + u64::from(stereo));
        let (ok, err) = hammer(dec.as_mut(), &mut rng, 400);
        eprintln!("{d}: {ok} ok, {err} errors");
    }
    // xHE-AAC has no parametric-stereo audio mode (MPS212 is signalled in the USAC
    // config instead); FDK rejects the SDC entry at configuration time.
    let mut bad = AudioInfo::xhe_aac(24_000, false, vec![0x00]).unwrap();
    bad.mode = AudioMode::ParametricStereo;
    assert!(FdkDrmDecoder::new(&bad).is_err());
    // Arbitrary config bytes must not crash configuration either.
    let mut rng = Rng(99);
    for _ in 0..50 {
        let n = 1 + rng.below(8) as usize;
        let junk = AudioInfo::xhe_aac(24_000, rng.below(2) == 1, rng.bytes(n)).unwrap();
        let _ = FdkDrmDecoder::new(&junk);
    }
}

#[test]
fn corrupted_aac_frames_are_concealed_and_recovered() {
    let cfg = FdkEncoderConfig::new(AacProfile::HeAac, 12_000, 16_000);
    let mut enc = FdkDrmEncoder::new(cfg).unwrap();
    let mut dec = FdkDrmDecoder::new(&enc.audio_info()).unwrap();
    let mut rng = Rng(7);
    let mut frames = Vec::new();
    for i in 0..60 {
        let pcm: Vec<f32> = (0..1920)
            .map(|j| {
                (0.4 * (2.0 * std::f64::consts::PI * 440.0 * (i * 1920 + j) as f64 / 24_000.0)
                    .sin()) as f32
            })
            .collect();
        if let Some(f) = enc.encode(&pcm).unwrap() {
            frames.push(f);
        }
    }
    let mut concealed_bad = 0;
    for (i, f) in frames.iter().enumerate() {
        let mut bytes = f.to_bytes();
        let mut crc = f.crc();
        let damaged = i >= 20 && i % 3 == 0;
        if damaged {
            match i % 4 {
                0 => crc ^= 0x5A, // wrong CRC
                1 => {
                    let k = rng.below(bytes.len() as u64) as usize; // bit error in the
                    bytes[k.min(4)] ^= 0x10; // CRC-protected start
                }
                2 => bytes.truncate(bytes.len() / 2), // truncated
                _ => bytes = rng.bytes(bytes.len()),  // replaced by noise
            }
        }
        let pcm = dec
            .decode(&bytes, Some(crc))
            .expect("damaged frames are concealed");
        sane(&pcm);
        if damaged && pcm.concealed {
            concealed_bad += 1;
        }
        if !damaged && i > 45 {
            assert!(
                !pcm.concealed,
                "frame {i} after the damage should decode cleanly"
            );
        }
    }
    eprintln!("{concealed_bad} damaged frames flagged as concealed");
    assert!(concealed_bad >= 8);
    // AAC without a CRC byte is a usage error.
    assert!(matches!(
        dec.decode(&frames[0].to_bytes(), None),
        Err(CodecError::InvalidInput(_))
    ));
}

#[test]
fn opus_decoder_survives_garbage_and_uses_fec_on_crc_errors() {
    let mut dec = OpusDrmDecoder::new().unwrap();
    let mut rng = Rng(3);
    let (ok, err) = hammer(&mut dec, &mut rng, 400);
    eprintln!("opus garbage: {ok} ok, {err} errors");
    assert_eq!(err, 0, "Opus garbage is always concealed");

    let mut enc = OpusDrmEncoder::new(OpusEncoderConfig {
        fec: true,
        ..OpusEncoderConfig::new(1, 60)
    })
    .unwrap();
    for i in 0..50 {
        let pcm: Vec<f32> = (0..960)
            .map(|j| 0.3 * ((i * 960 + j) as f32 * 0.05).sin())
            .collect();
        let f = enc.encode(&pcm).unwrap();
        let wrong = i % 5 == 4;
        let crc = if wrong { f.crc ^ 1 } else { f.crc };
        let out = dec.decode(&f.data, Some(crc)).unwrap();
        assert_eq!(out.concealed, wrong, "packet {i}");
        assert_eq!(out.frames(), 960);
        sane(&out);
    }
    // Empty frame = lost packet → PLC of the last duration.
    let plc = dec.decode(&[], Some(0)).unwrap();
    assert!(plc.concealed);
    assert_eq!(plc.frames(), 960);
}

#[test]
fn configuration_errors_are_reported() {
    // Coding mismatch between the request and the SDC bytes.
    let aac = AudioInfo::aac(12_000, false, AudioMode::Mono)
        .unwrap()
        .to_type9_bytes();
    assert!(open_decoder(DrmAudioCoding::XheAac, &aac).is_err());
    // Too short.
    assert!(open_decoder(DrmAudioCoding::Aac, &[0x00]).is_err());
    // Reserved AAC sampling-rate code 4.
    assert!(open_decoder(DrmAudioCoding::Aac, &[0x04, 0x00]).is_err());
    // Opus does not need SDC bytes.
    assert!(open_decoder(DrmAudioCoding::Opus, &[]).is_ok());
    // Encoder: unsupported core rate.
    assert!(FdkDrmEncoder::new(FdkEncoderConfig::new(AacProfile::Lc, 16_000, 16_000)).is_err());
    // Encoder: wrong input length.
    let mut enc =
        FdkDrmEncoder::new(FdkEncoderConfig::new(AacProfile::Lc, 12_000, 16_000)).unwrap();
    assert!(matches!(
        enc.encode(&[0.0; 10]),
        Err(CodecError::InvalidInput(_))
    ));
}

#[test]
fn fdk_reports_drm_and_usac_support() {
    let info = fdk_lib_info();
    eprintln!("{info:?}");
    assert!(info.drm && info.drm_sbr && info.parametric_stereo && info.usac && info.encoder_960);
    assert!(info.decoder_version.starts_with("3."));
}
