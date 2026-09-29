//! Throwaway end-to-end audio check (until the mux layer lands): hand-parse SDC types
//! 0 and 9, cut AAC super frames from stream 0, decode with FDK, write a WAV file.
//! Usage: audioprobe <in.flac> <out.wav>
use decdrm_codecs::{DrmAudioCoding, open_decoder};
use decdrm_core::bits::{BitReader, pack, unpack};
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::rx::{InputFormat, MscConfig, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_io::{AudioFormat, Container, Encoding, FileReader, FileWriter, To48k};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut reader = FileReader::open(&a[1]).unwrap();
    let fmt = reader.format();
    let mut to48 = To48k::new(fmt.sample_rate, fmt.channels).unwrap();
    let mut rx = Receiver::new(ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: fmt.channels, ..Default::default() });
    let mut fac = None;
    let mut streams: Vec<(usize, usize)> = Vec::new();
    let mut prot = (0usize, 0usize);
    let mut type9: Option<Vec<u8>> = None;
    let mut decoder = None;
    let mut writer: Option<FileWriter> = None;
    let (mut ok, mut conc, mut bad_sf) = (0, 0, 0);
    while let Some(chunk) = reader.read(4800).unwrap() {
        let s48 = to48.process(&chunk);
        for ev in rx.push(&s48) {
            match ev {
                ReceiverEvent::Fac(f) => fac = Some(f),
                ReceiverEvent::Sdc(b) if b.crc_ok && decoder.is_none() => {
                    let bits = unpack(&b.data);
                    let mut r = BitReader::from_bits(&bits);
                    while r.remaining() >= 16 {
                        let len = r.read(7) as usize;
                        let _v = r.read(1);
                        let ty = r.read(4);
                        if len == 0 && ty == 0 { break; }
                        let body_start = r.position();
                        match ty {
                            0 => {
                                prot = (r.read(2) as usize, r.read(2) as usize);
                                streams = (0..len / 3).map(|_| (r.read(12) as usize, r.read(12) as usize)).collect();
                            }
                            9 => {
                                let _ids = r.read(4);
                                let bytes: Vec<u8> = (0..len).map(|_| r.read(8) as u8).collect();
                                type9 = Some(bytes);
                            }
                            _ => {}
                        }
                        r = BitReader::from_bits(&bits);
                        r.skip(body_start + 4 + 8 * len);
                    }
                    if let (Some(f), Some(t9), false) = (fac, type9.clone(), streams.is_empty()) {
                        println!("streams {streams:?} prot {prot:?} type9 {:02x?}", t9);
                        rx.set_msc_config(Some(MscConfig {
                            mode: f.channel.msc_mode,
                            protection: MscProtection { part_a: prot.0, part_b: prot.1, hierarchical: 0 },
                            part_a_bytes: streams.iter().map(|s| s.0).sum(),
                            interleaving: f.channel.interleaving,
                        }));
                        let d = open_decoder(DrmAudioCoding::Aac, &t9).expect("open FDK");
                        println!("decoder: {}", d.describe());
                        decoder = Some((d, t9));
                    }
                }
                ReceiverEvent::Msc(m) => {
                    let Some((dec, t9)) = decoder.as_mut() else { continue };
                    let bytes = pack(&m.bits);
                    let hpp = m.hpp_bits / 8;
                    let (la, lb) = streams[0];
                    let mut s0 = bytes[..la].to_vec();
                    s0.extend_from_slice(&bytes[hpp..hpp + lb]);
                    // Sample rate code: 1 = 12 kHz (5 frames), 3 = 24 kHz (10 frames).
                    let rate_code = t9[0] & 0x07;
                    let n = if rate_code == 3 { 10 } else { 5 };
                    let header_bytes = (12 * (n - 1) + if n == 10 { 4 } else { 0 }) / 8;
                    let payload = la + lb - header_bytes - n;
                    let hb = unpack(&s0[..header_bytes]);
                    let mut hr = BitReader::from_bits(&hb);
                    let mut lens = Vec::new();
                    let mut prev = 0usize;
                    let mut good = true;
                    for _ in 0..n - 1 {
                        let mut b = hr.read(12) as usize;
                        if b < prev { b += 4096; }
                        if b > payload { good = false; }
                        lens.push(b - prev);
                        prev = b;
                    }
                    if prev > payload { good = false; }
                    lens.push(payload.saturating_sub(prev));
                    if !good { bad_sf += 1; continue; }
                    let hp = if la > 0 { (la - header_bytes - n) / n } else { 0 };
                    let mut pos = header_bytes;
                    let mut frames: Vec<Vec<u8>> = lens.iter().map(|&l| Vec::with_capacity(l)).collect();
                    let mut crcs = vec![0u8; n];
                    for f in 0..n {
                        frames[f].extend_from_slice(&s0[pos..pos + hp]);
                        pos += hp;
                        crcs[f] = s0[pos];
                        pos += 1;
                    }
                    for f in 0..n {
                        let lp = lens[f] - hp;
                        frames[f].extend_from_slice(&s0[pos..pos + lp]);
                        pos += lp;
                    }
                    for f in 0..n {
                        match dec.decode(&frames[f], Some(crcs[f])) {
                            Ok(pcm) => {
                                if pcm.concealed { conc += 1 } else { ok += 1 }
                                let w = writer.get_or_insert_with(|| {
                                    FileWriter::create(&a[2], AudioFormat::new(pcm.sample_rate, pcm.channels as usize), Container::Wav, Encoding::Int16).unwrap()
                                });
                                w.write(&pcm.samples).unwrap();
                            }
                            Err(e) => { conc += 1; if conc < 5 { println!("decode error: {e}"); } }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(w) = writer { w.finalize().unwrap(); }
    println!("AAC frames: ok {ok}, concealed/failed {conc}, bad super-frame headers {bad_sf}; status MER {:?}", rx.status().mer_db);
}
