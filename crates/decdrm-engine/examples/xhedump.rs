//! Diagnostic: the xHE-AAC audio frames of a recording, for comparing decoders.
//!
//! `cargo run --release -p decdrm-engine --example xhedump -- FILE [iq] OUT_PREFIX`
//!
//! Writes `OUT_PREFIX.type9` (the SDC type 9 entity body FDK is configured with),
//! `OUT_PREFIX.frames` (every audio frame of the first audio service: 2-byte big-endian
//! length, 1 byte "frame CRC-16 ok", the frame = USAC access unit + CRC-16) and
//! `OUT_PREFIX.fdk.f32` (FDK-AAC's output, raw interleaved f32).

use decdrm_codecs::{DrmAudioCoding, open_decoder};
use decdrm_core::mux::audio::AudioDeframer;
use decdrm_core::mux::demultiplex;
use decdrm_core::mux::service::{AudioCodec, Ensemble};
use decdrm_core::rx::{InputFormat, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_engine::{InputSpec, Source};
use std::io::Write as _;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: xhedump FILE [iq] OUT_PREFIX");
    let iq = args.iter().any(|a| a == "iq");
    let prefix = args.iter().skip(2).find(|a| *a != "iq").expect("OUT_PREFIX");
    let mut source = Source::open(&InputSpec::File { path: path.into(), realtime: false })?;
    let channels = source.info().channels;
    let input = if iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) };
    let mut rx = Receiver::new(ReceiverConfig { input, channels, ..Default::default() });
    let mut ens = Ensemble::new();
    let mut msc_config = None;
    let mut frames_out = std::fs::File::create(format!("{prefix}.frames"))?;
    let mut pcm_out = std::fs::File::create(format!("{prefix}.fdk.f32"))?;
    let mut audio = None;
    let (mut n, mut crc_bad) = (0usize, 0usize);
    while let Some(chunk) = source.read(2400)? {
        for ev in rx.push(&chunk) {
            match ev {
                ReceiverEvent::Fac(f) => {
                    ens.update_fac(&f);
                }
                ReceiverEvent::Sdc(b) if b.crc_ok => {
                    ens.update_sdc(&b.data);
                }
                ReceiverEvent::Msc(frame) => {
                    if !frame.complete {
                        continue;
                    }
                    let Some(mux) = ens.multiplex() else { continue };
                    let logical = demultiplex(&frame, mux);
                    if audio.is_none()
                        && let Some(p) = ens.services().find_map(|s| s.audio.clone())
                        && p.codec == AudioCodec::XheAac
                        && let Some(&stream) = ens.stream_lengths().get(p.stream_id as usize)
                    {
                        println!("type 9 ({} bytes): {:02x?}", p.type9_bytes.len(), p.type9_bytes);
                        println!("stream {} lengths {:?}", p.stream_id, stream);
                        std::fs::write(format!("{prefix}.type9"), &p.type9_bytes)?;
                        let dec = open_decoder(DrmAudioCoding::XheAac, &p.type9_bytes)?;
                        audio = Some((p.stream_id, AudioDeframer::new(&p, stream)?, dec));
                    }
                    let Some((sid, deframer, dec)) = audio.as_mut() else { continue };
                    let Some(Some(lf)) = logical.get(*sid as usize) else { continue };
                    let sf = deframer.push_frame(lf);
                    if let Some(e) = &sf.error {
                        println!("super frame error: {e}");
                    }
                    for f in &sf.frames {
                        let ok = f.crc_ok == Some(true);
                        crc_bad += usize::from(!ok);
                        frames_out.write_all(&(f.data.len() as u16).to_be_bytes())?;
                        frames_out.write_all(&[u8::from(ok)])?;
                        frames_out.write_all(&f.data)?;
                        let pcm = dec.decode(&f.data, None)?;
                        if n == 0 {
                            println!("FDK output: {} Hz, {} ch, {} samples/frame", pcm.sample_rate, pcm.channels, pcm.samples.len() / pcm.channels as usize);
                        }
                        for s in &pcm.samples {
                            pcm_out.write_all(&s.to_le_bytes())?;
                        }
                        n += 1;
                    }
                }
                _ => {}
            }
        }
        if ens.msc_config() != msc_config {
            msc_config = ens.msc_config();
            rx.set_msc_config(msc_config);
        }
    }
    println!("{n} frames, {crc_bad} with a bad or missing CRC-16");
    Ok(())
}
