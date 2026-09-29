//! Run the receiver on a FLAC recording and print what it finds.
//! Usage: cargo run -p decdrm-core --example rxfile -- <file.flac> [iq]
use decdrm_core::rx::{InputFormat, RealChannel, Receiver, ReceiverConfig, ReceiverEvent};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = &args[1];
    let iq = args.get(2).is_some_and(|a| a == "iq");
    let mut reader = claxon::FlacReader::open(path).expect("open flac");
    let info = reader.streaminfo();
    let ch = info.channels as usize;
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let rate = info.sample_rate;
    let samples: Vec<f32> = reader.samples().map(|s| s.unwrap() as f32 * scale).collect();
    // Sample-rate conversion to 48 kHz with the polyphase resampler.
    let samples = if rate != 48000 {
        let frames = samples.len() / ch;
        let ratio = 48000.0 / rate as f64;
        let mut chans: Vec<Vec<f32>> = Vec::new();
        for c in 0..ch {
            let x: Vec<decdrm_core::Cplx> = (0..frames).map(|i| decdrm_core::Cplx::new(samples[i * ch + c] as f64, 0.0)).collect();
            let mut r = decdrm_core::dsp::resampler::FracResampler::new();
            let mut y = Vec::new();
            r.process(&x, ratio, &mut y);
            chans.push(y.iter().map(|v| v.re as f32).collect());
        }
        let n = chans.iter().map(|c| c.len()).min().unwrap_or(0);
        let mut out = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in &chans {
                out.push(c[i]);
            }
        }
        out
    } else {
        samples
    };
    let cfg = ReceiverConfig {
        input: if iq { InputFormat::Iq { swap: false } } else { InputFormat::Real(RealChannel::Mix) },
        channels: ch,
        ..Default::default()
    };
    let mut rx = Receiver::new(cfg);
    let t0 = std::time::Instant::now();
    let mut fac_ok = 0;
    let mut fac_bad = 0;
    let mut last_print = 0usize;
    for (i, chunk) in samples.chunks(4800 * ch).enumerate() {
        for ev in rx.push(chunk) {
            let t = (i * 4800) as f64 / 48000.0;
            match ev {
                ReceiverEvent::Fac(f) => {
                    fac_ok += 1;
                    if fac_ok <= 3 || fac_ok % 25 == 0 {
                        println!("{t:7.2}s FAC ok: {:?}", f.channel);
                        println!("          service {:?}", f.service);
                    }
                }
                ReceiverEvent::FacError => fac_bad += 1,
                ReceiverEvent::Sdc(b) => println!("{t:7.2}s SDC crc_ok={} afs={} len={}", b.crc_ok, b.afs_index, b.data.len()),
                other => println!("{t:7.2}s {other:?}"),
            }
        }
        let t = (i * 4800) / 48000;
        if t >= last_print + 5 {
            last_print = t;
            let s = rx.status();
            println!("  [{t:3}s] facMER={:?}", s.fac_mer_db.map(|v| (v * 10.0).round() / 10.0));
            println!("  [{t:3}s] state={:?} mode={:?} so={:?} dc={:?} sro={:.2} snr={:?} dopp={:.2} delay={:.2} fsync={:?} fac {}/{}",
                s.state, s.mode, s.occupancy.map(|o| o.value()), s.dc_frequency_hz.map(|f| (f * 10.0).round() / 10.0), s.sro_hz,
                s.snr_db.map(|v| (v * 10.0).round() / 10.0), s.doppler_hz, s.delay_ms, s.frame_sync, fac_ok, fac_bad);
        }
    }
    let dur = samples.len() as f64 / ch as f64 / 48000.0;
    println!("done: {dur:.1}s of audio in {:.2}s; FAC ok {fac_ok}, bad {fac_bad}", t0.elapsed().as_secs_f64());
}
