//! Diagnostic: the xHE-AAC audio frames of a recording, for comparing decoders.
//!
//! `cargo run --release -p decdrm-engine --example xhedump -- FILE [iq] OUT_PREFIX`
//!
//! Writes, for the first audio service:
//! * `OUT_PREFIX.type9`: the SDC type 9 entity body FDK-AAC is configured with;
//! * `OUT_PREFIX.frames`: every audio frame as a 2-byte big-endian length, 1 byte "frame
//!   CRC-16 ok" and the frame (USAC access unit + CRC-16);
//! * `OUT_PREFIX.fdk.wav`: FDK-AAC's output;
//! * `OUT_PREFIX.usac` and `OUT_PREFIX.meta`: the same access units for libxaac's test
//!   decoder, which knows USAC but not DRM: the MPEG-4 AudioSpecificConfig converted from
//!   the xHE-AAC Static Config (`decdrm_codecs::audio_specific_config_from_drm`) followed
//!   by the access units without their CRC-16, and their sizes in its metadata format.
//!
//! Build libxaac's test decoder from the submodule and decode with it (with a
//! multi-configuration generator such as Visual Studio, the program lands in `Release/`):
//!
//! ```text
//! cmake -S third_party/libxaac -B target/libxaac -DCMAKE_BUILD_TYPE=Release
//! cmake --build target/libxaac --config Release --target xaacdec
//! target/libxaac/xaacdec -ifile:OUT_PREFIX.usac -imeta:OUT_PREFIX.meta -ofile:OUT_PREFIX.xaac.wav -mp4:1 -pcmsz:16
//! ```
//!
//! The two outputs then differ by a constant delay (libxaac's is 160 samples ahead for
//! CNR-1's mono 8:3 SBR stream).

use decdrm_codecs::{AudioInfo, DrmAudioCoding, audio_specific_config_from_drm, open_decoder};
use decdrm_core::mux::audio::AudioDeframer;
use decdrm_core::mux::demultiplex;
use decdrm_core::mux::service::{AudioCodec, Ensemble};
use decdrm_core::rx::{InputFormat, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_engine::{InputSpec, Source};
use decdrm_io::{AudioFormat, Container, Encoding, FileWriter};
use std::fmt::Write as _;
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
    let mut usac_out = std::fs::File::create(format!("{prefix}.usac"))?;
    let mut wav: Option<FileWriter> = None;
    let mut audio = None;
    // Length of the AudioSpecificConfig, output rate, access unit sizes (for the metadata).
    let mut asc_len = 0;
    let mut rate = 0;
    let mut au_sizes = Vec::new();
    let mut crc_bad = 0usize;
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
                        let info = AudioInfo::from_type9_bytes(&p.type9_bytes)?;
                        let asc = audio_specific_config_from_drm(&info)?;
                        println!("MPEG-4 AudioSpecificConfig: {asc:02x?}");
                        usac_out.write_all(&asc)?;
                        (asc_len, rate) = (asc.len(), info.sample_rate().unwrap_or(0));
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
                        let au = &f.data[..f.data.len().saturating_sub(2)];
                        usac_out.write_all(au)?;
                        au_sizes.push(au.len());
                        let pcm = dec.decode(&f.data, None)?;
                        let w = match &mut wav {
                            Some(w) => w,
                            None => {
                                println!("FDK output: {} Hz, {} ch, {} samples/frame", pcm.sample_rate, pcm.channels, pcm.samples.len() / pcm.channels as usize);
                                let format = AudioFormat::new(pcm.sample_rate, usize::from(pcm.channels));
                                wav.insert(FileWriter::create(format!("{prefix}.fdk.wav"), format, Container::Wav, Encoding::Int16)?)
                            }
                        };
                        w.write(&pcm.samples)?;
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
    if let Some(w) = wav {
        w.finalize()?;
    }
    // libxaac's metadata (test/decoder/ixheaacd_metadata_read.c): the AudioSpecificConfig
    // at the start of the .usac file, then the access unit sizes.
    let mut meta = format!(
        "-dec_info_init:{asc_len}\n-g_track_count:1\n-movie_time_scale:{rate}\n\
         -media_time_scale:{rate}\n-ia_mp4_stsz_entries:{}\n",
        au_sizes.len()
    );
    for n in &au_sizes {
        writeln!(meta, "-ia_mp4_stsz_size:{n}")?;
    }
    std::fs::write(format!("{prefix}.meta"), meta)?;
    println!("{} frames, {crc_bad} with a bad or missing CRC-16", au_sizes.len());
    Ok(())
}
