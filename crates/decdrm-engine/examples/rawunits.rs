//! Diagnostic: the SDC description of every data application and the data units of the
//! applications DecDRM does not interpret, for working out unknown formats.
//!
//! `cargo run --release -p decdrm-engine --example rawunits -- FILE [iq] [OUT]`
//!
//! Prints the first units in hex; with `OUT`, writes every unit to that file, each
//! preceded by its length (2 bytes, big endian) and its user application id (2 bytes).
//! Also tries to read each unit's data group payload as an xHE-AAC audio super frame
//! (§5.3.1) and reports how often that works.

use decdrm_core::mux::audio::XheAacDeframer;
use decdrm_core::rx::{InputFormat, RealChannel, ReceiverConfig};
use decdrm_data::datagroup::DataGroup;
use decdrm_data::DataEvent;
use decdrm_engine::{InputSpec, Session, SessionEvent, Source};
use std::io::Write as _;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: rawunits FILE [iq] [OUT]");
    let iq = args.iter().any(|a| a == "iq");
    let out_path = args.iter().skip(2).find(|a| *a != "iq");
    let mut out = out_path.map(std::fs::File::create).transpose()?;
    let mut source = Source::open(&InputSpec::File { path: path.into(), realtime: false })?;
    let channels = source.info().channels;
    let input = if iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) };
    let mut session = Session::new(ReceiverConfig { input, channels, ..Default::default() });
    let (mut units, mut stream_frames) = (0usize, 0usize);
    let mut sizes = std::collections::BTreeMap::<usize, usize>::new();
    let mut xhe = XheAacDeframer::new();
    let (mut xhe_ok, mut xhe_crc, mut xhe_frames) = (0usize, 0usize, 0usize);
    while let Some(frames) = source.read(2400)? {
        for ev in session.push(&frames) {
            match ev {
                SessionEvent::Data { short_id, event: DataEvent::Raw { user_app_id, data_group } } => {
                    if units < 6 {
                        println!(
                            "{:7.2}s service {short_id} app {user_app_id:#05x}: unit of {} bytes: {}",
                            session.time_s(),
                            data_group.len(),
                            hex(&data_group[..data_group.len().min(48)])
                        );
                    }
                    *sizes.entry(data_group.len()).or_default() += 1;
                    if let Ok(dg) = DataGroup::parse(&data_group) {
                        match xhe.push(&dg.data) {
                            Ok((h, frames)) => {
                                xhe_ok += 1;
                                xhe_crc += usize::from(h.header_crc_ok);
                                xhe_frames += frames.len();
                                if units < 6 {
                                    let lens: Vec<usize> = frames.iter().map(|f| f.data.len()).collect();
                                    println!("    as xHE-AAC: {} borders, reservoir level {}, header CRC {}, frames {lens:?}", h.frame_border_count, h.bit_reservoir_level, if h.header_crc_ok { "ok" } else { "bad" });
                                }
                            }
                            Err(e) if units < 6 => println!("    as xHE-AAC: {e}"),
                            Err(_) => {}
                        }
                    }
                    if let Some(f) = out.as_mut() {
                        f.write_all(&(data_group.len() as u16).to_be_bytes())?;
                        f.write_all(&user_app_id.to_be_bytes())?;
                        f.write_all(&data_group)?;
                    }
                    units += 1;
                }
                SessionEvent::Data { event: DataEvent::StreamData { data, .. }, .. } => {
                    if stream_frames < 3 {
                        println!("stream frame of {} bytes: {}", data.len(), hex(&data[..data.len().min(48)]));
                    }
                    stream_frames += 1;
                }
                _ => {}
            }
        }
    }
    let ens = session.ensemble();
    for s in ens.services() {
        println!("service {}: FAC {:?}", s.short_id, s.fac);
        println!("  label {:?}, language {:?}, country {:?}, audio {:?}", s.label, s.language_code, s.country_code, s.audio.as_ref().map(|a| a.codec));
        for a in &s.applications {
            println!(
                "  application: stream {} packet mode {} unit indicator {} packet id {} length {} domain {} data [{}]",
                a.stream_id,
                a.packet_mode,
                a.data_unit_indicator,
                a.packet_id,
                a.packet_length,
                a.app_domain,
                hex(&a.application_data)
            );
        }
    }
    println!("streams: {:?}", ens.stream_lengths());
    println!("{units} data units (sizes: {sizes:?}), {stream_frames} stream frames");
    println!("as xHE-AAC super frames: {xhe_ok} parsed, {xhe_crc} with a good header CRC, {xhe_frames} audio frames");
    Ok(())
}
