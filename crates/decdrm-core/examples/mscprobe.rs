//! Quick MSC sanity check: hand-parse SDC type 0, enable MSC decoding and test
//! whether AAC super-frame headers in stream 0 are self-consistent.
use decdrm_core::bits::BitReader;
use decdrm_core::fec::mlc::MscProtection;
use decdrm_core::rx::{InputFormat, MscConfig, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut reader = claxon::FlacReader::open(&path).unwrap();
    let info = reader.streaminfo();
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let samples: Vec<f32> = reader.samples().map(|s| s.unwrap() as f32 * scale).collect();
    let mut rx = Receiver::new(ReceiverConfig { input: InputFormat::Real(RealChannel::Mix), channels: 1, ..Default::default() });
    let mut fac = None;
    let mut streams: Vec<(usize, usize)> = Vec::new();
    let mut configured = false;
    let (mut ok_hdr, mut bad_hdr) = (0, 0);
    for chunk in samples.chunks(4800) {
        for ev in rx.push(chunk) {
            match ev {
                ReceiverEvent::Fac(f) => fac = Some(f),
                ReceiverEvent::Sdc(b) if b.crc_ok && streams.is_empty() => {
                    // Walk the entities looking for type 0.
                    let bits = decdrm_core::bits::unpack(&b.data);
                    let mut r = BitReader::from_bits(&bits);
                    while r.remaining() >= 16 {
                        let len = r.read(7) as usize;
                        let _ver = r.read(1);
                        let ty = r.read(4);
                        let body_bits = 4 + 8 * len;
                        if len == 0 && ty == 0 { break; }
                        if ty == 0 {
                            let pa = r.read(2) as usize;
                            let pb = r.read(2) as usize;
                            for _ in 0..len / 3 {
                                let a = r.read(12) as usize;
                                let bb = r.read(12) as usize;
                                streams.push((a, bb));
                            }
                            println!("multiplex: prot A {pa} B {pb} streams {streams:?}");
                            if let Some(f) = fac {
                                let part_a: usize = streams.iter().map(|s| s.0).sum();
                                rx.set_msc_config(Some(MscConfig {
                                    mode: f.channel.msc_mode,
                                    protection: MscProtection { part_a: pa, part_b: pb, hierarchical: 0 },
                                    part_a_bytes: part_a,
                                    interleaving: f.channel.interleaving,
                                }));
                                configured = true;
                            }
                        } else {
                            r.skip(body_bits);
                        }
                    }
                }
                ReceiverEvent::Msc(m) if configured => {
                    let bytes_all = decdrm_core::bits::pack(&m.bits);
                    let hpp_bytes = m.hpp_bits / 8;
                    let lpp_total = bytes_all.len() - hpp_bytes;
                    // Stream 0 = first len_a bytes of HPP and first len_b bytes of LPP.
                    let (la, lb) = streams[0];
                    let mut s0 = bytes_all[..la].to_vec();
                    s0.extend_from_slice(&bytes_all[hpp_bytes..hpp_bytes + lb.min(lpp_total)]);
                    // Try 10 frames (24 kHz core): 9 borders * 12 bits + 4.
                    let bits = decdrm_core::bits::unpack(&s0);
                    let mut r = BitReader::from_bits(&bits);
                    let mut prev = 0usize;
                    let mut okb = true;
                    let payload = s0.len() - 14 - 10 - 4; // minus header, CRC bytes, text message bytes
                    for _ in 0..9 {
                        let b = r.read(12) as usize;
                        if b < prev || b > payload { okb = false; }
                        prev = b;
                    }
                    if okb { ok_hdr += 1 } else { bad_hdr += 1 }
                }
                _ => {}
            }
        }
    }
    println!("AAC super-frame headers (24 kHz/10 frames hypothesis): consistent {ok_hdr}, inconsistent {bad_hdr}; status {:?}", rx.status().mer_db);
}
