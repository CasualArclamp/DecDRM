//! Diagnostic: every Journaline data group of a recording, as received, for working out
//! how a station builds its pages (escape codes and extended headers included, which
//! the decoder resolves or drops).
//!
//! `cargo run --release -p decdrm-engine --example jmldump -- FILE [iq] [OUT]`
//!
//! Prints each data group (time, data group header fields, NML header, the body with
//! NML codes and escapes shown as `<xx>`) and the parsed page. With `OUT`, writes every
//! data group field to that file, each preceded by the time in milliseconds (4 bytes)
//! and its length (2 bytes), big endian.

use decdrm_core::mux::demultiplex;
use decdrm_core::mux::service::Ensemble;
use decdrm_core::rx::{InputFormat, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};
use decdrm_data::datagroup::DataGroup;
use decdrm_data::journaline::NmlObject;
use decdrm_data::packet::PacketDemux;
use decdrm_data::{AppDomain, DataServiceConfig, UserApplication};
use decdrm_engine::{InputSpec, Source};
use std::io::Write as _;

/// Bytes as text, NML codes and escapes (below 0x20) as `<xx>`.
fn show(bytes: &[u8]) -> String {
    let mut s = String::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if (c as u32) < 0x20 {
                s.push_str(&format!("<{:02x}>", c as u32));
            } else {
                s.push(c);
            }
        }
        for b in chunk.invalid() {
            s.push_str(&format!("<!{b:02x}>"));
        }
    }
    s
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: jmldump FILE [iq] [OUT]");
    let iq = args.iter().any(|a| a == "iq");
    let out_path = args.iter().skip(2).find(|a| *a != "iq");
    let mut out = out_path.map(std::fs::File::create).transpose()?;
    let mut source = Source::open(&InputSpec::File { path: path.into(), realtime: false })?;
    let channels = source.info().channels;
    let input = if iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) };
    let mut rx = Receiver::new(ReceiverConfig { input, channels, ..Default::default() });
    let mut ens = Ensemble::new();
    // (stream id, packet id, demultiplexer) of each Journaline application.
    let mut apps: Vec<(u8, u8, PacketDemux)> = Vec::new();
    let mut samples = 0usize;
    let mut groups = 0usize;
    while let Some(frames) = source.read(2400)? {
        samples += frames.len() / channels.max(1);
        let t = samples as f64 / 48_000.0;
        for ev in rx.push(&frames) {
            match ev {
                ReceiverEvent::Fac(fac) => {
                    ens.update_fac(&fac);
                    rx.set_msc_config(ens.msc_config());
                }
                ReceiverEvent::Sdc(b) if b.crc_ok => {
                    ens.update_sdc(&b.data);
                    rx.set_msc_config(ens.msc_config());
                    for s in ens.services() {
                        for a in &s.applications {
                            let id = a.user_app_id().unwrap_or(0);
                            let known = apps.iter().any(|(st, p, _)| *st == a.stream_id && *p == a.packet_id);
                            if !known && a.packet_mode && UserApplication::from_id(AppDomain::from_sdc(a.app_domain), id) == UserApplication::Journaline {
                                let len = DataServiceConfig::total_packet_len(a.packet_length);
                                println!(
                                    "{t:7.2}s Journaline on service {} (label {:?}): stream {}, packet id {}, packet length {len}, \
                                     data unit indicator {}, app data {:02x?}",
                                    s.short_id,
                                    s.label,
                                    a.stream_id,
                                    a.packet_id,
                                    a.data_unit_indicator,
                                    a.user_app_data()
                                );
                                apps.push((a.stream_id, a.packet_id, PacketDemux::new(len)?));
                            }
                        }
                    }
                }
                ReceiverEvent::Msc(frame) => {
                    let Some(mux) = ens.multiplex() else { continue };
                    let logical = demultiplex(&frame, mux);
                    for (stream, pid, demux) in &mut apps {
                        let Some(Some(lf)) = logical.get(*stream as usize) else { continue };
                        for unit in demux.push_frame(&lf.data).into_iter().filter(|u| u.packet_id == *pid) {
                            let Ok(dg) = DataGroup::parse(&unit.data) else {
                                println!("{t:7.2}s data group damaged ({} bytes)", unit.data.len());
                                continue;
                            };
                            groups += 1;
                            if let Some(f) = &mut out {
                                f.write_all(&((t * 1000.0) as u32).to_be_bytes())?;
                                f.write_all(&(dg.data.len() as u16).to_be_bytes())?;
                                f.write_all(&dg.data)?;
                            }
                            let d = &dg.data;
                            if d.len() < 3 {
                                println!("{t:7.2}s short data group {d:02x?}");
                                continue;
                            }
                            println!(
                                "{t:7.2}s group type {} cont {} rep {} crc {} ext {:?} seg {} access {} | id {:#06x} type {} static {} compressed {} rev {} | {} bytes: {}",
                                dg.group_type,
                                dg.continuity,
                                dg.repetition,
                                dg.with_crc,
                                dg.extension,
                                dg.segment.is_some(),
                                dg.user_access.is_some(),
                                u16::from_be_bytes([d[0], d[1]]),
                                d[2] >> 5,
                                (d[2] >> 4) & 1,
                                (d[2] >> 3) & 1,
                                d[2] & 7,
                                d.len(),
                                show(&d[3..])
                            );
                            match NmlObject::parse(d, 0) {
                                Ok(o) => println!("          {o:?}"),
                                Err(e) => println!("          parse error: {e}"),
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    println!("{groups} Journaline data groups in {t:.1} s", t = samples as f64 / 48_000.0);
    Ok(())
}
